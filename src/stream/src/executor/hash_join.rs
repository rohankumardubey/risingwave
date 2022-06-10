// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::VecDeque;
use std::pin::Pin;

use futures::stream::FuturesOrdered;
use futures::StreamExt;
use futures_async_stream::try_stream;
use itertools::Itertools;
use madsim::collections::HashSet;
use risingwave_common::array::{Array, ArrayRef, Op, Row, RowRef, StreamChunk};
use risingwave_common::catalog::Schema;
use risingwave_common::error::{internal_error, Result, RwError};
use risingwave_common::hash::HashKey;
use risingwave_common::types::{DataType, ToOwnedDatum};
use risingwave_expr::expr::RowExpression;
use risingwave_storage::{Keyspace, StateStore};

use super::barrier_align::*;
use super::error::StreamExecutorError;
use super::managed_state::join::*;
use super::{BoxedExecutor, BoxedMessageStream, Executor, Message, PkIndices, PkIndicesRef};
use crate::common::StreamChunkBuilder;
use crate::executor::PROCESSING_WINDOW_SIZE;

pub const JOIN_CACHE_SIZE: usize = 1 << 16;

/// The `JoinType` and `SideType` are to mimic a enum, because currently
/// enum is not supported in const generic.
// TODO: Use enum to replace this once [feature(adt_const_params)](https://github.com/rust-lang/rust/issues/95174) get completed.
pub type JoinTypePrimitive = u8;
#[allow(non_snake_case, non_upper_case_globals)]
pub mod JoinType {
    use super::JoinTypePrimitive;
    pub const Inner: JoinTypePrimitive = 0;
    pub const LeftOuter: JoinTypePrimitive = 1;
    pub const RightOuter: JoinTypePrimitive = 2;
    pub const FullOuter: JoinTypePrimitive = 3;
    pub const LeftSemi: JoinTypePrimitive = 4;
    pub const LeftAnti: JoinTypePrimitive = 5;
    pub const RightSemi: JoinTypePrimitive = 6;
    pub const RightAnti: JoinTypePrimitive = 7;
}

pub type SideTypePrimitive = u8;
#[allow(non_snake_case, non_upper_case_globals)]
pub mod SideType {
    use super::SideTypePrimitive;
    pub const Left: SideTypePrimitive = 0;
    pub const Right: SideTypePrimitive = 1;
}

#[derive(Clone, Hash, PartialEq, Eq)]
enum KeyType<K> {
    Left(K),
    Right(K),
}

const fn is_outer_side(join_type: JoinTypePrimitive, side_type: SideTypePrimitive) -> bool {
    join_type == JoinType::FullOuter
        || (join_type == JoinType::LeftOuter && side_type == SideType::Left)
        || (join_type == JoinType::RightOuter && side_type == SideType::Right)
}

const fn outer_side_null(join_type: JoinTypePrimitive, side_type: SideTypePrimitive) -> bool {
    join_type == JoinType::FullOuter
        || (join_type == JoinType::LeftOuter && side_type == SideType::Right)
        || (join_type == JoinType::RightOuter && side_type == SideType::Left)
}

/// Send the update only once if the join type is semi/anti and the update is the same side as the
/// join
const fn forward_exactly_once(join_type: JoinTypePrimitive, side_type: SideTypePrimitive) -> bool {
    ((join_type == JoinType::LeftSemi || join_type == JoinType::LeftAnti)
        && side_type == SideType::Left)
        || ((join_type == JoinType::RightSemi || join_type == JoinType::RightAnti)
            && side_type == SideType::Right)
}

const fn only_forward_matched_side(
    join_type: JoinTypePrimitive,
    side_type: SideTypePrimitive,
) -> bool {
    ((join_type == JoinType::LeftSemi || join_type == JoinType::LeftAnti)
        && side_type == SideType::Right)
        || ((join_type == JoinType::RightSemi || join_type == JoinType::RightAnti)
            && side_type == SideType::Left)
}

const fn is_semi(join_type: JoinTypePrimitive) -> bool {
    join_type == JoinType::LeftSemi || join_type == JoinType::RightSemi
}

const fn is_anti(join_type: JoinTypePrimitive) -> bool {
    join_type == JoinType::LeftAnti || join_type == JoinType::RightAnti
}

const fn is_semi_or_anti(join_type: JoinTypePrimitive) -> bool {
    is_semi(join_type) || is_anti(join_type)
}

pub struct JoinParams {
    /// Indices of the join columns
    pub key_indices: Vec<usize>,
}

impl JoinParams {
    pub fn new(key_indices: Vec<usize>) -> Self {
        Self { key_indices }
    }
}

struct JoinSide<K: HashKey, S: StateStore> {
    /// Store all data from a one side stream
    ht: JoinHashMap<K, S>,
    /// Indices of the join key columns
    key_indices: Vec<usize>,
    /// The primary key indices of this side, used for state store
    pk_indices: Vec<usize>,
    /// The date type of each columns to join on
    col_types: Vec<DataType>,
    /// The start position for the side in output new columns
    start_pos: usize,
    /// The join side operates on this keyspace.
    keyspace: Keyspace<S>,
}

impl<K: HashKey, S: StateStore> std::fmt::Debug for JoinSide<K, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinSide")
            .field("key_indices", &self.key_indices)
            .field("pk_indices", &self.pk_indices)
            .field("col_types", &self.col_types)
            .field("start_pos", &self.start_pos)
            .finish()
    }
}

impl<K: HashKey, S: StateStore> JoinSide<K, S> {
    fn is_dirty(&self) -> bool {
        self.ht.values().any(|state| state.is_dirty())
    }

    #[allow(dead_code)]
    fn clear_cache(&mut self) {
        assert!(
            !self.is_dirty(),
            "cannot clear cache while states of hash join are dirty"
        );

        // TODO: not working with rearranged chain
        // self.ht.clear();
    }
}

/// `HashJoinExecutor` takes two input streams and runs equal hash join on them.
/// The output columns are the concatenation of left and right columns.
pub struct HashJoinExecutor<K: HashKey, S: StateStore, const T: JoinTypePrimitive> {
    /// Left input executor.
    input_l: Option<BoxedExecutor>,
    /// Right input executor.
    input_r: Option<BoxedExecutor>,
    /// the data types of the formed new columns
    output_data_types: Vec<DataType>,
    /// The schema of the hash join executor
    schema: Schema,
    /// The primary key indices of the schema
    pk_indices: PkIndices,
    /// The parameters of the left join executor
    side_l: JoinSide<K, S>,
    /// The parameters of the right join executor
    side_r: JoinSide<K, S>,
    /// Optional non-equi join conditions
    cond: Option<RowExpression>,
    /// Identity string
    identity: String,
    /// Epoch
    epoch: u64,

    #[allow(dead_code)]
    /// Logical Operator Info
    op_info: String,

    #[allow(dead_code)]
    /// Indices of the columns on which key distribution depends.
    key_indices: Vec<usize>,

    /// Whether the logic can be optimized for append-only stream
    append_only_optimize: bool,

    /// Depth of the I/O prefetch queue
    prefetch_queue_depth: usize,

    /// Number of msgs seen before adjusting AIMD flow control. Set to u64::MAX to disable AIMD.
    aimd_adjust_rate_msgs: u64,
}

impl<K: HashKey, S: StateStore, const T: JoinTypePrimitive> std::fmt::Debug
    for HashJoinExecutor<K, S, T>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HashJoinExecutor")
            .field("join_type", &T)
            .field("input_left", &self.input_l.as_ref().unwrap().identity())
            .field("input_right", &self.input_r.as_ref().unwrap().identity())
            .field("side_l", &self.side_l)
            .field("side_r", &self.side_r)
            .field("pk_indices", &self.pk_indices)
            .field("schema", &self.schema)
            .field("output_data_types", &self.output_data_types)
            .finish()
    }
}

impl<K: HashKey, S: StateStore, const T: JoinTypePrimitive> Executor for HashJoinExecutor<K, S, T> {
    fn execute(self: Box<Self>) -> BoxedMessageStream {
        self.into_stream().boxed()
    }

    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn pk_indices(&self) -> PkIndicesRef {
        &self.pk_indices
    }

    fn identity(&self) -> &str {
        self.identity.as_str()
    }
}

struct HashJoinChunkBuilder<const T: JoinTypePrimitive, const SIDE: SideTypePrimitive> {
    stream_chunk_builder: StreamChunkBuilder,
}

impl<const T: JoinTypePrimitive, const SIDE: SideTypePrimitive> HashJoinChunkBuilder<T, SIDE> {
    fn with_match_on_insert(
        &mut self,
        row: &RowRef,
        matched_row: &mut JoinRow,
    ) -> Result<Option<StreamChunk>> {
        // Left/Right Anti sides
        if is_anti(T) {
            if matched_row.is_zero_degree() && only_forward_matched_side(T, SIDE) {
                self.stream_chunk_builder
                    .append_row_matched(Op::Delete, &matched_row.row)
            } else {
                Ok(None)
            }
        // Left/Right Semi sides
        } else if is_semi(T) {
            if matched_row.is_zero_degree() && only_forward_matched_side(T, SIDE) {
                self.stream_chunk_builder
                    .append_row_matched(Op::Insert, &matched_row.row)
            } else {
                Ok(None)
            }
        // Outer sides
        } else if matched_row.is_zero_degree() && outer_side_null(T, SIDE) {
            // if the matched_row does not have any current matches
            // `StreamChunkBuilder` guarantees that `UpdateDelete` will never
            // issue an output chunk.
            if self
                .stream_chunk_builder
                .append_row_matched(Op::UpdateDelete, &matched_row.row)?
                .is_some()
            {
                return Err(internal_error("`Op::UpdateDelete` should not yield chunk"));
            }
            self.stream_chunk_builder
                .append_row(Op::UpdateInsert, row, &matched_row.row)
        // Inner sides
        } else {
            self.stream_chunk_builder
                .append_row(Op::Insert, row, &matched_row.row)
        }
    }

    fn with_match_on_delete(
        &mut self,
        row: &RowRef,
        matched_row: &mut JoinRow,
    ) -> Result<Option<StreamChunk>> {
        // Left/Right Anti sides
        if is_anti(T) {
            if matched_row.is_zero_degree() && only_forward_matched_side(T, SIDE) {
                self.stream_chunk_builder
                    .append_row_matched(Op::Insert, &matched_row.row)
            } else {
                Ok(None)
            }
        // Left/Right Semi sides
        } else if is_semi(T) {
            if matched_row.is_zero_degree() && only_forward_matched_side(T, SIDE) {
                self.stream_chunk_builder
                    .append_row_matched(Op::Delete, &matched_row.row)
            } else {
                Ok(None)
            }
        // Outer sides
        } else if matched_row.is_zero_degree() && outer_side_null(T, SIDE) {
            // if the matched_row does not have any current
            // matches
            if self
                .stream_chunk_builder
                .append_row_matched(Op::UpdateDelete, &matched_row.row)?
                .is_some()
            {
                return Err(internal_error("`Op::UpdateDelete` should not yield chunk"));
            }
            self.stream_chunk_builder
                .append_row_matched(Op::UpdateInsert, &matched_row.row)
        // Inner sides
        } else {
            // concat with the matched_row and append the new
            // row
            // FIXME: we always use `Op::Delete` here to avoid
            // violating
            // the assumption for U+ after U-.
            self.stream_chunk_builder
                .append_row(Op::Delete, row, &matched_row.row)
        }
    }

    #[inline]
    fn forward_exactly_once_if_matched(
        &mut self,
        op: Op,
        row: &RowRef,
    ) -> Result<Option<StreamChunk>> {
        // if it's a semi join and the side needs to be maintained.
        if is_semi(T) && forward_exactly_once(T, SIDE) {
            self.stream_chunk_builder.append_row_update(op, row)
        } else {
            Ok(None)
        }
    }

    #[inline]
    fn forward_if_not_matched(&mut self, op: Op, row: &RowRef) -> Result<Option<StreamChunk>> {
        // if it's outer join or anti join and the side needs to be maintained.
        if (is_anti(T) && forward_exactly_once(T, SIDE)) || is_outer_side(T, SIDE) {
            self.stream_chunk_builder.append_row_update(op, row)
        } else {
            Ok(None)
        }
    }

    #[inline]
    fn take(&mut self) -> Result<Option<StreamChunk>> {
        self.stream_chunk_builder.take()
    }
}

impl<K: HashKey, S: StateStore, const T: JoinTypePrimitive> HashJoinExecutor<K, S, T> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        input_l: BoxedExecutor,
        input_r: BoxedExecutor,
        params_l: JoinParams,
        params_r: JoinParams,
        pk_indices: PkIndices,
        executor_id: u64,
        cond: Option<RowExpression>,
        op_info: String,
        key_indices: Vec<usize>,
        ks_l: Keyspace<S>,
        ks_r: Keyspace<S>,
        append_only: bool,
        prefetch_queue_depth: usize,
        aimd_adjust_rate_msgs: u64,
    ) -> Self {
        let side_l_column_n = input_l.schema().len();

        let schema_fields = match T {
            JoinType::LeftSemi | JoinType::LeftAnti => input_l.schema().fields.clone(),
            JoinType::RightSemi | JoinType::RightAnti => input_r.schema().fields.clone(),
            _ => [
                input_l.schema().fields.clone(),
                input_r.schema().fields.clone(),
            ]
            .concat(),
        };

        let output_data_types = schema_fields
            .iter()
            .map(|field| field.data_type.clone())
            .collect();
        let col_l_datatypes = input_l
            .schema()
            .fields
            .iter()
            .map(|field| field.data_type.clone())
            .collect_vec();
        let col_r_datatypes = input_r
            .schema()
            .fields
            .iter()
            .map(|field| field.data_type.clone())
            .collect_vec();

        let pk_indices_l = input_l.pk_indices().to_vec();
        let pk_indices_r = input_r.pk_indices().to_vec();

        // check whether join key contains pk in both side
        let append_only_optimize = if append_only {
            let join_key_l = HashSet::<usize>::from_iter(params_l.key_indices.clone());
            let join_key_r = HashSet::<usize>::from_iter(params_r.key_indices.clone());
            let pk_contained_l = pk_indices_l.len()
                == pk_indices_l
                    .iter()
                    .filter(|x| join_key_l.contains(x))
                    .count();
            let pk_contained_r = pk_indices_r.len()
                == pk_indices_r
                    .iter()
                    .filter(|x| join_key_r.contains(x))
                    .count();
            pk_contained_l && pk_contained_r
        } else {
            false
        };

        Self {
            input_l: Some(input_l),
            input_r: Some(input_r),
            output_data_types,
            schema: Schema {
                fields: schema_fields,
            },
            side_l: JoinSide {
                ht: JoinHashMap::new(
                    JOIN_CACHE_SIZE,
                    pk_indices_l.clone(),
                    params_l.key_indices.clone(),
                    col_l_datatypes.clone(),
                    ks_l.clone(),
                ), // TODO: decide the target cap
                key_indices: params_l.key_indices,
                col_types: col_l_datatypes,
                pk_indices: pk_indices_l,
                start_pos: 0,
                keyspace: ks_l,
            },
            side_r: JoinSide {
                ht: JoinHashMap::new(
                    JOIN_CACHE_SIZE,
                    pk_indices_r.clone(),
                    params_r.key_indices.clone(),
                    col_r_datatypes.clone(),
                    ks_r.clone(),
                ), // TODO: decide the target cap
                key_indices: params_r.key_indices,
                col_types: col_r_datatypes,
                pk_indices: pk_indices_r,
                start_pos: side_l_column_n,
                keyspace: ks_r,
            },
            pk_indices,
            cond,
            identity: format!("HashJoinExecutor {:X}", executor_id),
            op_info,
            key_indices,
            epoch: 0,
            append_only_optimize,
            prefetch_queue_depth,
            aimd_adjust_rate_msgs,
        }
    }

    fn prefetch_message(
        &mut self,
        msg: AlignedMessage,
        io_queue: &mut FuturesOrdered<
            Pin<Box<dyn futures::Future<Output = (KeyType<K>, JoinEntryState<S>)> + Send>>,
        >,
        inflight_io_set: &mut HashSet<KeyType<K>>,
    ) -> Result<(bool, AlignedMessage, HashSet<KeyType<K>>)> {
        let mut msg_io_set = HashSet::new();
        match msg {
            AlignedMessage::Left(chunk) => {
                let chunk = chunk.compact()?;
                let (data_chunk, ops) = chunk.into_parts();
                let keys = K::build(&self.side_l.key_indices, &data_chunk)?;

                for key in &keys {
                    // We fetch the keys on the right side
                    if self.side_r.ht.get_cached(&key).is_none() {
                        msg_io_set.insert(KeyType::Right(key.clone()));
                        // This is the first time fetching the key
                        if inflight_io_set.insert(KeyType::Right(key.clone())) {
                            let key = key.clone();
                            let table_info = self.side_r.ht.table_info();
                            io_queue.push(Box::pin(async move {
                                let state = JoinHashMap::fetch_cached_state_inner(
                                    &key,
                                    table_info.as_ref(),
                                )
                                .await
                                .unwrap()
                                .unwrap_or_else(|| {
                                    JoinHashMap::init_with_empty_cache_inner(
                                        &key,
                                        table_info.as_ref(),
                                    )
                                    .unwrap()
                                });

                                (KeyType::Right(key), state)
                            }));
                        }
                    }
                }
                Ok((
                    false,
                    AlignedMessage::Left(StreamChunk::from_parts(ops, data_chunk)),
                    msg_io_set,
                ))
            }
            AlignedMessage::Right(chunk) => {
                let chunk = chunk.compact()?;
                let (data_chunk, ops) = chunk.into_parts();
                let keys = K::build(&self.side_r.key_indices, &data_chunk)?;

                for key in &keys {
                    // We fetch the keys on the left side
                    if self.side_l.ht.get_cached(&key).is_none() {
                        msg_io_set.insert(KeyType::Left(key.clone()));
                        // This is the first time fetching the key
                        if inflight_io_set.insert(KeyType::Left(key.clone())) {
                            let key = key.clone();
                            let table_info = self.side_l.ht.table_info();
                            io_queue.push(Box::pin(async move {
                                let state = JoinHashMap::fetch_cached_state_inner(
                                    &key,
                                    table_info.as_ref(),
                                )
                                .await
                                .unwrap()
                                .unwrap_or_else(|| {
                                    JoinHashMap::init_with_empty_cache_inner(
                                        &key,
                                        table_info.as_ref(),
                                    )
                                    .unwrap()
                                });
                                (KeyType::Left(key), state)
                            }));
                        }
                    }
                }
                Ok((
                    false,
                    AlignedMessage::Right(StreamChunk::from_parts(ops, data_chunk)),
                    msg_io_set,
                ))
            }
            AlignedMessage::Barrier(_) => Ok((true, msg, msg_io_set)),
        }
    }

    fn new_io_queue() -> FuturesOrdered<
        Pin<Box<dyn futures::Future<Output = (KeyType<K>, JoinEntryState<S>)> + Send>>,
    > {
        FuturesOrdered::new()
    }

    #[try_stream(ok = Message, error = StreamExecutorError)]
    async fn into_stream(mut self) {
        let input_l = self.input_l.take().unwrap();
        let input_r = self.input_r.take().unwrap();
        let aligned_stream = barrier_align(input_l.execute(), input_r.execute());
        futures::pin_mut!(aligned_stream);

        let mut msg_queue = VecDeque::<(AlignedMessage, HashSet<KeyType<K>>)>::new();
        let mut inflight_io_set = HashSet::<KeyType<K>>::new();
        let io_queue = Self::new_io_queue();
        futures::pin_mut!(io_queue);
        let always_ready = futures::future::ready(());

        let mut max_queue_depth = self.prefetch_queue_depth;

        let mut stream_ended = false;
        let mut barrier_in_queue = false;
        let mut processed_msg = false;
        let mut backpressure = false;
        let mut consecutive_no_wait = 0;
        let mut msg_uuid: u64 = 0;

        const ADDITIVE_INCREASE: usize = 2;
        const MULTIPLICATIVE_DECREASE: f64 = 0.75;
        const IDLE_SLEEP_TIME_US: u64 = 500;

        loop {
            // If queue is not full and stream has not ended, try to pull messages from upstream,
            // scheduling any necessary I/O for prefetching.
            while msg_queue.len() < max_queue_depth && !barrier_in_queue && !stream_ended {
                match futures::future::select(aligned_stream.next(), always_ready.clone()).await {
                    futures::future::Either::Left((maybe_msg, _)) => {
                        if let Some(msg) = maybe_msg {
                            let msg = msg?;
                            let (is_barrier, msg, msg_io_set) = self
                                .prefetch_message(msg, &mut io_queue, &mut inflight_io_set)
                                .map_err(StreamExecutorError::hash_join_error)?;
                            // barrier_in_queue = is_barrier;
                            inflight_io_set.extend(&mut msg_io_set.clone().into_iter());
                            msg_queue.push_front((msg, msg_io_set));
                        } else {
                            stream_ended = true;
                            break;
                        }
                    }
                    // If the queue from upstream is not ready, break for now.
                    // We will try to yield messages or sleep until there are new messages
                    futures::future::Either::Right(_) => break,
                }
            }

            if msg_uuid % 1024 == 0 {
                println!(
                    "max queue depth: {:?}, msg queue len: {:?}, consecutive_no_wait {}, inflight_ios: {}",
                    max_queue_depth,
                    msg_queue.len(),
                    consecutive_no_wait,
                    inflight_io_set.len()
                );
            }

            if processed_msg && msg_uuid % self.aimd_adjust_rate_msgs == 0 {
                processed_msg = false;
                if backpressure {
                    backpressure = false;
                    println!("MD");
                    // There is downstream backpressure, so we decrease the queue depth.
                    max_queue_depth =
                        (max_queue_depth as f64 * MULTIPLICATIVE_DECREASE).ceil() as usize;
                } else if msg_queue.len() == max_queue_depth {
                    println!("AI");
                    // there is source side pressure, so we increase the queue depth.
                    max_queue_depth += ADDITIVE_INCREASE;
                }
            }

            // Poll the IO futures so that they make progress.
            // If the next message is ready, process it and yield its output.
            if let Some((msg, msg_io_set)) = msg_queue.pop_back() {
                loop {
                    // While I/O are complete, remove them from the in-flight io set.
                    match futures::future::select(io_queue.next(), always_ready.clone()).await {
                        futures::future::Either::Left((maybe_io_result, _)) => {
                            if let Some((key_type, state)) = maybe_io_result {
                                // assert that the key was in the set
                                assert!(inflight_io_set.remove(&key_type));
                                match key_type {
                                    KeyType::Right(key) => {
                                        self.side_r.ht.insert_cached(key, state);
                                    }
                                    KeyType::Left(key) => {
                                        self.side_l.ht.insert_cached(key, state);
                                    }
                                }
                            } else {
                                // The I/O queue is exhausted
                                break;
                            }
                        }
                        // The I/O futures have made no progress, break for now.
                        // We will try to yield messages or sleep until there are new messages
                        futures::future::Either::Right(_) => break,
                    }
                }

                if inflight_io_set.is_disjoint(&msg_io_set) {
                    msg_uuid += 1;
                    processed_msg = true;
                    consecutive_no_wait += 1;
                    if consecutive_no_wait > self.prefetch_queue_depth {
                        backpressure = true;
                    }
                    // Process the message and yield output chunks
                    match msg {
                        AlignedMessage::Left(chunk) => {
                            #[for_await]
                            for chunk in self.eq_join_oneside::<{ SideType::Left }>(chunk) {
                                yield chunk.map_err(StreamExecutorError::hash_join_error)?;
                            }
                        }
                        AlignedMessage::Right(chunk) => {
                            #[for_await]
                            for chunk in self.eq_join_oneside::<{ SideType::Right }>(chunk) {
                                yield chunk.map_err(StreamExecutorError::hash_join_error)?;
                            }
                        }
                        AlignedMessage::Barrier(barrier) => {
                            // we need to have processed all inflight I/Os by the time we process a
                            // barrier
                            // assert!(inflight_io_set.is_empty());
                            self.flush_data()
                                .await
                                .map_err(StreamExecutorError::hash_join_error)?;
                            let epoch = barrier.epoch.curr;
                            self.side_l.ht.update_epoch(epoch);
                            self.side_r.ht.update_epoch(epoch);
                            self.epoch = epoch;
                            barrier_in_queue = false;
                            yield Message::Barrier(barrier);
                        }
                    }
                } else {
                    consecutive_no_wait = 0;
                    // Message is not yet ready
                    msg_queue.push_back((msg, msg_io_set));
                    tokio::time::sleep(std::time::Duration::from_micros(IDLE_SLEEP_TIME_US)).await;
                }
            } else {
                if stream_ended {
                    break; // end the stream
                } else {
                    // There are no messages to yield from the queue and no messages ready from
                    // upstream. Sleep and try polling upstream again.
                    tokio::time::sleep(std::time::Duration::from_micros(IDLE_SLEEP_TIME_US)).await;
                }
            }
        }
    }

    async fn flush_data(&mut self) -> Result<()> {
        let epoch = self.epoch;
        for side in [&mut self.side_l, &mut self.side_r] {
            let mut write_batch = side.keyspace.state_store().start_write_batch();
            for state in side.ht.values_mut() {
                state.flush(&mut write_batch)?;
            }
            write_batch.ingest(epoch).await.unwrap();
        }

        // evict the LRU cache
        assert!(!self.side_l.is_dirty());
        self.side_l.ht.evict_to_target_cap();
        assert!(!self.side_r.is_dirty());
        self.side_r.ht.evict_to_target_cap();
        Ok(())
    }

    /// the data the hash table and match the coming
    /// data chunk with the executor state
    async fn hash_eq_match<'a>(
        key: &'a K,
        ht: &'a mut JoinHashMap<K, S>,
    ) -> Option<&'a mut JoinEntryState<S>> {
        if key.has_null() {
            None
        } else {
            ht.get_mut(key).await
        }
    }

    fn row_concat(
        row_update: &RowRef<'_>,
        update_start_pos: usize,
        row_matched: &Row,
        matched_start_pos: usize,
    ) -> Row {
        let mut new_row = vec![None; row_update.size() + row_matched.size()];

        for (i, datum_ref) in row_update.values().enumerate() {
            new_row[i + update_start_pos] = datum_ref.to_owned_datum();
        }
        for i in 0..row_matched.size() {
            new_row[i + matched_start_pos] = row_matched[i].clone();
        }
        Row(new_row)
    }

    fn bool_from_array_ref(array_ref: ArrayRef) -> bool {
        let bool_array = array_ref.as_ref().as_bool();
        bool_array.value_at(0).unwrap_or_else(|| {
            panic!(
                "Some thing wrong with the expression result. Bool array: {:?}",
                bool_array
            )
        })
    }

    #[try_stream(ok = Message, error = RwError)]
    async fn eq_join_oneside<const SIDE: SideTypePrimitive>(&mut self, chunk: StreamChunk) {
        let epoch = self.epoch;
        let output_data_types = &self.output_data_types;
        let mut side_l = &mut self.side_l;
        let mut side_r = &mut self.side_r;
        let cond = &mut self.cond;
        let append_only_optimize = self.append_only_optimize;

        // Compaction is already performed during the prefetch phase.
        let (data_chunk, ops) = chunk.into_parts();

        let (side_update, side_match) = if SIDE == SideType::Left {
            (&mut side_l, &mut side_r)
        } else {
            (&mut side_r, &mut side_l)
        };

        let (update_start_pos, matched_start_pos) = if is_semi_or_anti(T) {
            (0, 0)
        } else {
            (side_update.start_pos, side_match.start_pos)
        };

        let mut hashjoin_chunk_builder = HashJoinChunkBuilder::<T, SIDE> {
            stream_chunk_builder: StreamChunkBuilder::new(
                PROCESSING_WINDOW_SIZE,
                output_data_types,
                update_start_pos,
                matched_start_pos,
            )?,
        };

        let mut check_join_condition = |row_update: &RowRef<'_>,
                                        row_matched: &Row|
         -> Result<bool> {
            // TODO(yuhao-su): We should find a better way to eval the
            // expression without concat
            // two rows.
            let mut cond_match = true;
            // if there are non-equi expressions
            if let Some(ref mut cond) = cond {
                let new_row =
                    Self::row_concat(row_update, update_start_pos, row_matched, matched_start_pos);

                cond_match = Self::bool_from_array_ref(cond.eval(&new_row, output_data_types)?);
            }
            Ok(cond_match)
        };

        let keys = K::build(&side_update.key_indices, &data_chunk)?;
        for (idx, (row, op)) in data_chunk.rows().zip_eq(ops.iter()).enumerate() {
            let key = &keys[idx];
            let value = row.to_owned_row();
            let pk = row.row_by_indices(&side_update.pk_indices);
            let mut matched_rows = Self::hash_eq_match(key, &mut side_match.ht).await;
            match *op {
                Op::Insert | Op::UpdateInsert => {
                    let entry_value = side_update.ht.get_or_init_without_cache(key).await?;
                    let mut degree = 0;
                    let mut matched_pks: Vec<Row> = Vec::with_capacity(1);
                    if let Some(matched_rows) = matched_rows.as_mut() {
                        for matched_row in (*matched_rows).values_mut(epoch).await {
                            if check_join_condition(&row, &matched_row.row)? {
                                degree += 1;
                                if !forward_exactly_once(T, SIDE) {
                                    if let Some(chunk) = hashjoin_chunk_builder
                                        .with_match_on_insert(&row, matched_row)?
                                    {
                                        yield Message::Chunk(chunk);
                                    }
                                }
                                matched_row.inc_degree();
                            }
                            // If the stream is append-only and the join key covers pk in both side,
                            // then we can remove matched rows since pk is unique and will not be
                            // inserted again
                            if append_only_optimize {
                                let pk_matched = matched_row.row_by_indices(&side_match.pk_indices);
                                matched_pks.push(pk_matched);
                            }
                        }
                        if degree == 0 {
                            if let Some(chunk) =
                                hashjoin_chunk_builder.forward_if_not_matched(*op, &row)?
                            {
                                yield Message::Chunk(chunk);
                            }
                        } else if let Some(chunk) =
                            hashjoin_chunk_builder.forward_exactly_once_if_matched(*op, &row)?
                        {
                            yield Message::Chunk(chunk);
                        }
                    } else if let Some(chunk) =
                        hashjoin_chunk_builder.forward_if_not_matched(*op, &row)?
                    {
                        yield Message::Chunk(chunk);
                    }

                    if append_only_optimize {
                        match matched_rows {
                            Some(v) => {
                                // Since join key contains pk and pk is unique, there should be only
                                // one row if matched
                                if matched_pks.len() > 0 {
                                    debug_assert!(1 == matched_pks.len());
                                    v.remove(matched_pks.remove(0));
                                } else {
                                    entry_value.insert(pk, JoinRow::new(value, degree));
                                }
                            }
                            None => {
                                entry_value.insert(pk, JoinRow::new(value, degree));
                            }
                        }
                    } else {
                        entry_value.insert(pk, JoinRow::new(value, degree));
                    }
                }
                Op::Delete | Op::UpdateDelete => {
                    if let Some(v) = side_update.ht.get_mut_without_cached(key).await? {
                        // remove the row by it's primary key
                        v.remove(pk);
                    }

                    if let Some(matched_rows) = matched_rows {
                        let mut matched = false;
                        for matched_row in matched_rows.values_mut(epoch).await {
                            if check_join_condition(&row, &matched_row.row)? {
                                matched = true;
                                matched_row.dec_degree()?;
                                if !forward_exactly_once(T, SIDE) {
                                    if let Some(chunk) = hashjoin_chunk_builder
                                        .with_match_on_delete(&row, matched_row)?
                                    {
                                        yield Message::Chunk(chunk);
                                    }
                                }
                            }
                        }
                        if !matched {
                            if let Some(chunk) =
                                hashjoin_chunk_builder.forward_if_not_matched(*op, &row)?
                            {
                                yield Message::Chunk(chunk);
                            }
                        } else if let Some(chunk) =
                            hashjoin_chunk_builder.forward_exactly_once_if_matched(*op, &row)?
                        {
                            yield Message::Chunk(chunk);
                        }
                    } else if let Some(chunk) =
                        hashjoin_chunk_builder.forward_if_not_matched(*op, &row)?
                    {
                        yield Message::Chunk(chunk);
                    }
                }
            }
        }
        if let Some(chunk) = hashjoin_chunk_builder.take()? {
            yield Message::Chunk(chunk);
        }
    }
}

#[cfg(test)]
mod tests {
    use risingwave_common::array::stream_chunk::StreamChunkTestExt;
    use risingwave_common::array::*;
    use risingwave_common::catalog::{Field, Schema, TableId};
    use risingwave_common::hash::{Key128, Key64};
    use risingwave_expr::expr::expr_binary_nonnull::new_binary_expr;
    use risingwave_expr::expr::{InputRefExpression, RowExpression};
    use risingwave_pb::expr::expr_node::Type;
    use risingwave_storage::memory::MemoryStateStore;

    use super::{HashJoinExecutor, JoinParams, JoinType, *};
    use crate::executor::test_utils::{MessageSender, MockSource};
    use crate::executor::{Barrier, Epoch, Message};

    fn create_in_memory_keyspace() -> (Keyspace<MemoryStateStore>, Keyspace<MemoryStateStore>) {
        let mem_state = MemoryStateStore::new();
        (
            Keyspace::table_root(mem_state.clone(), &TableId::new(0)),
            Keyspace::table_root(mem_state, &TableId::new(1)),
        )
    }

    fn create_cond() -> RowExpression {
        let left_expr = InputRefExpression::new(DataType::Int64, 1);
        let right_expr = InputRefExpression::new(DataType::Int64, 3);
        let cond = new_binary_expr(
            Type::LessThan,
            DataType::Boolean,
            Box::new(left_expr),
            Box::new(right_expr),
        );
        RowExpression::new(cond)
    }

    fn create_executor<const T: JoinTypePrimitive>(
        with_condition: bool,
    ) -> (MessageSender, MessageSender, BoxedMessageStream) {
        let schema = Schema {
            fields: vec![
                Field::unnamed(DataType::Int64), // join key
                Field::unnamed(DataType::Int64),
            ],
        };
        let (tx_l, source_l) = MockSource::channel(schema.clone(), vec![0, 1]);
        let (tx_r, source_r) = MockSource::channel(schema, vec![0, 1]);
        let params_l = JoinParams::new(vec![0]);
        let params_r = JoinParams::new(vec![0]);
        let cond = with_condition.then(create_cond);

        let (ks_l, ks_r) = create_in_memory_keyspace();

        let executor = HashJoinExecutor::<Key64, MemoryStateStore, T>::new(
            Box::new(source_l),
            Box::new(source_r),
            params_l,
            params_r,
            vec![1],
            1,
            cond,
            "HashJoinExecutor".to_string(),
            vec![],
            ks_l,
            ks_r,
            false,
            64,
            u64::MAX,
        );
        (tx_l, tx_r, Box::new(executor).execute())
    }

    fn create_append_only_executor<const T: JoinTypePrimitive>(
        with_condition: bool,
    ) -> (MessageSender, MessageSender, BoxedMessageStream) {
        let schema = Schema {
            fields: vec![
                Field::unnamed(DataType::Int64),
                Field::unnamed(DataType::Int64),
                Field::unnamed(DataType::Int64),
            ],
        };
        let (tx_l, source_l) = MockSource::channel(schema.clone(), vec![0]);
        let (tx_r, source_r) = MockSource::channel(schema, vec![0]);
        let params_l = JoinParams::new(vec![0, 1]);
        let params_r = JoinParams::new(vec![0, 1]);
        let cond = with_condition.then(create_cond);

        let (ks_l, ks_r) = create_in_memory_keyspace();

        let executor = HashJoinExecutor::<Key128, MemoryStateStore, T>::new(
            Box::new(source_l),
            Box::new(source_r),
            params_l,
            params_r,
            vec![1],
            1,
            cond,
            "HashJoinExecutor".to_string(),
            vec![],
            ks_l,
            ks_r,
            true,
            64,
            u64::MAX,
        );
        (tx_l, tx_r, Box::new(executor).execute())
    }

    #[tokio::test]
    async fn test_streaming_hash_inner_join() {
        let chunk_l1 = StreamChunk::from_pretty(
            "  I I
             + 1 4
             + 2 5
             + 3 6",
        );
        let chunk_l2 = StreamChunk::from_pretty(
            "  I I
             + 3 8
             - 3 8",
        );
        let chunk_r1 = StreamChunk::from_pretty(
            "  I I
             + 2 7
             + 4 8
             + 6 9",
        );
        let chunk_r2 = StreamChunk::from_pretty(
            "  I  I
             + 3 10
             + 6 11",
        );
        let (mut tx_l, mut tx_r, mut hash_join) = create_executor::<{ JoinType::Inner }>(false);

        // push the init barrier for left and right
        tx_l.push_barrier(1, false);
        tx_r.push_barrier(1, false);
        hash_join.next().await.unwrap().unwrap();

        // push the 1st left chunk
        tx_l.push_chunk(chunk_l1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty("I I I I")
        );

        // push the init barrier for left and right
        tx_l.push_barrier(1, false);
        tx_r.push_barrier(1, false);
        hash_join.next().await.unwrap().unwrap();

        // push the 2nd left chunk
        tx_l.push_chunk(chunk_l2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty("I I I I")
        );

        // push the 1st right chunk
        tx_r.push_chunk(chunk_r1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I I I
                + 2 5 2 7"
            )
        );

        // push the 2nd right chunk
        tx_r.push_chunk(chunk_r2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I I I
                + 3 6 3 10"
            )
        );
    }

    #[tokio::test]
    async fn test_streaming_hash_left_semi_join() {
        let chunk_l1 = StreamChunk::from_pretty(
            "  I I
             + 1 4
             + 2 5
             + 3 6",
        );
        let chunk_l2 = StreamChunk::from_pretty(
            "  I I
             + 3 8
             - 3 8",
        );
        let chunk_r1 = StreamChunk::from_pretty(
            "  I I
             + 2 7
             + 4 8
             + 6 9",
        );
        let chunk_r2 = StreamChunk::from_pretty(
            "  I  I
             + 3 10
             + 6 11",
        );
        let chunk_l3 = StreamChunk::from_pretty(
            "  I I
             + 6 10",
        );
        let chunk_r3 = StreamChunk::from_pretty(
            "  I  I
             - 6 11",
        );
        let chunk_r4 = StreamChunk::from_pretty(
            "  I  I
             - 6 9",
        );
        let (mut tx_l, mut tx_r, mut hash_join) = create_executor::<{ JoinType::LeftSemi }>(false);

        // push the init barrier for left and right
        tx_l.push_barrier(1, false);
        tx_r.push_barrier(1, false);
        hash_join.next().await.unwrap().unwrap();

        // push the 1st left chunk
        tx_l.push_chunk(chunk_l1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(chunk.into_chunk().unwrap(), StreamChunk::from_pretty("I I"));

        // push the init barrier for left and right
        tx_l.push_barrier(1, false);
        tx_r.push_barrier(1, false);
        hash_join.next().await.unwrap().unwrap();

        // push the 2nd left chunk
        tx_l.push_chunk(chunk_l2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(chunk.into_chunk().unwrap(), StreamChunk::from_pretty("I I"));

        // push the 1st right chunk
        tx_r.push_chunk(chunk_r1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I
                + 2 5"
            )
        );

        // push the 2nd right chunk
        tx_r.push_chunk(chunk_r2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I
                + 3 6"
            )
        );

        // push the 3rd left chunk (tests forward_exactly_once)
        tx_l.push_chunk(chunk_l3);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I
                + 6 10"
            )
        );

        // push the 3rd right chunk
        // (tests that no change if there are still matches)
        tx_r.push_chunk(chunk_r3);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(chunk.into_chunk().unwrap(), StreamChunk::from_pretty("I I"));

        // push the 3rd left chunk
        // (tests that deletion occurs when there are no more matches)
        tx_r.push_chunk(chunk_r4);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I
                - 6 10"
            )
        );
    }

    #[tokio::test]
    async fn test_streaming_hash_inner_join_append_only() {
        let chunk_l1 = StreamChunk::from_pretty(
            "  I I I
             + 1 4 1
             + 2 5 2
             + 3 6 3",
        );
        let chunk_l2 = StreamChunk::from_pretty(
            "  I I I
             + 4 9 4
             + 5 10 5",
        );
        let chunk_r1 = StreamChunk::from_pretty(
            "  I I I
             + 2 5 1
             + 4 9 2
             + 6 9 3",
        );
        let chunk_r2 = StreamChunk::from_pretty(
            "  I I I
             + 1 4 4
             + 3 6 5",
        );

        let (mut tx_l, mut tx_r, mut hash_join) =
            create_append_only_executor::<{ JoinType::Inner }>(false);

        // push the init barrier for left and right
        tx_l.push_barrier(1, false);
        tx_r.push_barrier(1, false);
        hash_join.next().await.unwrap().unwrap();

        // push the 1st left chunk
        tx_l.push_chunk(chunk_l1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty("I I I I I I")
        );

        // push the init barrier for left and right
        tx_l.push_barrier(1, false);
        tx_r.push_barrier(1, false);
        hash_join.next().await.unwrap().unwrap();

        // push the 2nd left chunk
        tx_l.push_chunk(chunk_l2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty("I I I I I I")
        );

        // push the 1st right chunk
        tx_r.push_chunk(chunk_r1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I I I I I
                + 2 5 2 2 5 1
                + 4 9 4 4 9 2"
            )
        );

        // push the 2nd right chunk
        tx_r.push_chunk(chunk_r2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I I I I I
                + 1 4 1 1 4 4
                + 3 6 3 3 6 5"
            )
        );
    }

    #[tokio::test]
    async fn test_streaming_hash_left_semi_join_append_only() {
        let chunk_l1 = StreamChunk::from_pretty(
            "  I I I
             + 1 4 1
             + 2 5 2
             + 3 6 3",
        );
        let chunk_l2 = StreamChunk::from_pretty(
            "  I I I
             + 4 9 4
             + 5 10 5",
        );
        let chunk_r1 = StreamChunk::from_pretty(
            "  I I I
             + 2 5 1
             + 4 9 2
             + 6 9 3",
        );
        let chunk_r2 = StreamChunk::from_pretty(
            "  I I I
             + 1 4 4
             + 3 6 5",
        );

        let (mut tx_l, mut tx_r, mut hash_join) =
            create_append_only_executor::<{ JoinType::LeftSemi }>(false);

        // push the init barrier for left and right
        tx_l.push_barrier(1, false);
        tx_r.push_barrier(1, false);
        hash_join.next().await.unwrap().unwrap();

        // push the 1st left chunk
        tx_l.push_chunk(chunk_l1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty("I I I")
        );

        // push the init barrier for left and right
        tx_l.push_barrier(1, false);
        tx_r.push_barrier(1, false);
        hash_join.next().await.unwrap().unwrap();

        // push the 2nd left chunk
        tx_l.push_chunk(chunk_l2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty("I I I")
        );

        // push the 1st right chunk
        tx_r.push_chunk(chunk_r1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I I
                + 2 5 2
                + 4 9 4"
            )
        );

        // push the 2nd right chunk
        tx_r.push_chunk(chunk_r2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I I
                + 1 4 1
                + 3 6 3"
            )
        );
    }

    #[tokio::test]
    async fn test_streaming_hash_right_semi_join_append_only() {
        let chunk_l1 = StreamChunk::from_pretty(
            "  I I I
             + 1 4 1
             + 2 5 2
             + 3 6 3",
        );
        let chunk_l2 = StreamChunk::from_pretty(
            "  I I I
             + 4 9 4
             + 5 10 5",
        );
        let chunk_r1 = StreamChunk::from_pretty(
            "  I I I
             + 2 5 1
             + 4 9 2
             + 6 9 3",
        );
        let chunk_r2 = StreamChunk::from_pretty(
            "  I I I
             + 1 4 4
             + 3 6 5",
        );

        let (mut tx_l, mut tx_r, mut hash_join) =
            create_append_only_executor::<{ JoinType::RightSemi }>(false);

        // push the init barrier for left and right
        tx_l.push_barrier(1, false);
        tx_r.push_barrier(1, false);
        hash_join.next().await.unwrap().unwrap();

        // push the 1st left chunk
        tx_l.push_chunk(chunk_l1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty("I I I")
        );

        // push the init barrier for left and right
        tx_l.push_barrier(1, false);
        tx_r.push_barrier(1, false);
        hash_join.next().await.unwrap().unwrap();

        // push the 2nd left chunk
        tx_l.push_chunk(chunk_l2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty("I I I")
        );

        // push the 1st right chunk
        tx_r.push_chunk(chunk_r1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I I
                + 2 5 1
                + 4 9 2"
            )
        );

        // push the 2nd right chunk
        tx_r.push_chunk(chunk_r2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I I
                + 1 4 4
                + 3 6 5"
            )
        );
    }

    #[tokio::test]
    async fn test_streaming_hash_right_semi_join() {
        let chunk_r1 = StreamChunk::from_pretty(
            "  I I
             + 1 4
             + 2 5
             + 3 6",
        );
        let chunk_r2 = StreamChunk::from_pretty(
            "  I I
             + 3 8
             - 3 8",
        );
        let chunk_l1 = StreamChunk::from_pretty(
            "  I I
             + 2 7
             + 4 8
             + 6 9",
        );
        let chunk_l2 = StreamChunk::from_pretty(
            "  I  I
             + 3 10
             + 6 11",
        );
        let chunk_r3 = StreamChunk::from_pretty(
            "  I I
             + 6 10",
        );
        let chunk_l3 = StreamChunk::from_pretty(
            "  I  I
             - 6 11",
        );
        let chunk_l4 = StreamChunk::from_pretty(
            "  I  I
             - 6 9",
        );
        let (mut tx_l, mut tx_r, mut hash_join) = create_executor::<{ JoinType::RightSemi }>(false);

        // push the init barrier for left and right
        tx_l.push_barrier(1, false);
        tx_r.push_barrier(1, false);
        hash_join.next().await.unwrap().unwrap();

        // push the 1st right chunk
        tx_r.push_chunk(chunk_r1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(chunk.into_chunk().unwrap(), StreamChunk::from_pretty("I I"));

        // push the init barrier for left and right
        tx_l.push_barrier(1, false);
        tx_r.push_barrier(1, false);
        hash_join.next().await.unwrap().unwrap();

        // push the 2nd right chunk
        tx_r.push_chunk(chunk_r2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(chunk.into_chunk().unwrap(), StreamChunk::from_pretty("I I"));

        // push the 1st left chunk
        tx_l.push_chunk(chunk_l1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I
                + 2 5"
            )
        );

        // push the 2nd left chunk
        tx_l.push_chunk(chunk_l2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I
                + 3 6"
            )
        );

        // push the 3rd right chunk (tests forward_exactly_once)
        tx_r.push_chunk(chunk_r3);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I
                + 6 10"
            )
        );

        // push the 3rd left chunk
        // (tests that no change if there are still matches)
        tx_l.push_chunk(chunk_l3);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(chunk.into_chunk().unwrap(), StreamChunk::from_pretty("I I"));

        // push the 3rd right chunk
        // (tests that deletion occurs when there are no more matches)
        tx_l.push_chunk(chunk_l4);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I
                - 6 10"
            )
        );
    }

    #[tokio::test]
    async fn test_streaming_hash_left_anti_join() {
        let chunk_l1 = StreamChunk::from_pretty(
            "  I I
             + 1 4
             + 2 5
             + 3 6",
        );
        let chunk_l2 = StreamChunk::from_pretty(
            "  I I
             + 3 8
             - 3 8",
        );
        let chunk_r1 = StreamChunk::from_pretty(
            "  I I
             + 2 7
             + 4 8
             + 6 9",
        );
        let chunk_r2 = StreamChunk::from_pretty(
            "  I  I
             + 3 10
             + 6 11
             + 1 2
             + 1 3",
        );
        let chunk_l3 = StreamChunk::from_pretty(
            "  I I
             + 9 10",
        );
        let chunk_r3 = StreamChunk::from_pretty(
            "  I I
             - 1 2",
        );
        let chunk_r4 = StreamChunk::from_pretty(
            "  I I
             - 1 3",
        );
        let (mut tx_l, mut tx_r, mut hash_join) = create_executor::<{ JoinType::LeftAnti }>(false);

        // push the init barrier for left and right
        tx_l.push_barrier(1, false);
        tx_r.push_barrier(1, false);
        hash_join.next().await.unwrap().unwrap();

        // push the 1st left chunk
        tx_l.push_chunk(chunk_l1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I
                + 1 4
                + 2 5
                + 3 6",
            )
        );

        // push the init barrier for left and right
        tx_l.push_barrier(1, false);
        tx_r.push_barrier(1, false);
        hash_join.next().await.unwrap().unwrap();

        // push the 2nd left chunk
        tx_l.push_chunk(chunk_l2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                "  I I
                 + 3 8
                 - 3 8",
            )
        );

        // push the 1st right chunk
        tx_r.push_chunk(chunk_r1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I
                - 2 5"
            )
        );

        // push the 2nd right chunk
        tx_r.push_chunk(chunk_r2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I
                - 3 6
                - 1 4"
            )
        );

        // push the 3rd left chunk (tests forward_exactly_once)
        tx_l.push_chunk(chunk_l3);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I
                + 9 10"
            )
        );

        // push the 3rd right chunk
        // (tests that no change if there are still matches)
        tx_r.push_chunk(chunk_r3);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(chunk.into_chunk().unwrap(), StreamChunk::from_pretty("I I"));

        // push the 4th right chunk
        // (tests that insertion occurs when there are no more matches)
        tx_r.push_chunk(chunk_r4);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I
                + 1 4"
            )
        );
    }

    #[tokio::test]
    async fn test_streaming_hash_right_anti_join() {
        let chunk_r1 = StreamChunk::from_pretty(
            "  I I
             + 1 4
             + 2 5
             + 3 6",
        );
        let chunk_r2 = StreamChunk::from_pretty(
            "  I I
             + 3 8
             - 3 8",
        );
        let chunk_l1 = StreamChunk::from_pretty(
            "  I I
             + 2 7
             + 4 8
             + 6 9",
        );
        let chunk_l2 = StreamChunk::from_pretty(
            "  I  I
             + 3 10
             + 6 11
             + 1 2
             + 1 3",
        );
        let chunk_r3 = StreamChunk::from_pretty(
            "  I I
             + 9 10",
        );
        let chunk_l3 = StreamChunk::from_pretty(
            "  I I
             - 1 2",
        );
        let chunk_l4 = StreamChunk::from_pretty(
            "  I I
             - 1 3",
        );
        let (mut tx_r, mut tx_l, mut hash_join) = create_executor::<{ JoinType::LeftAnti }>(false);

        // push the init barrier for left and right
        tx_r.push_barrier(1, false);
        tx_l.push_barrier(1, false);
        hash_join.next().await.unwrap().unwrap();

        // push the 1st right chunk
        tx_r.push_chunk(chunk_r1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I
                + 1 4
                + 2 5
                + 3 6",
            )
        );

        // push the init barrier for left and right
        tx_r.push_barrier(1, false);
        tx_l.push_barrier(1, false);
        hash_join.next().await.unwrap().unwrap();

        // push the 2nd right chunk
        tx_r.push_chunk(chunk_r2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                "  I I
                 + 3 8
                 - 3 8",
            )
        );

        // push the 1st left chunk
        tx_l.push_chunk(chunk_l1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I
                - 2 5"
            )
        );

        // push the 2nd left chunk
        tx_l.push_chunk(chunk_l2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I
                - 3 6
                - 1 4"
            )
        );

        // push the 3rd right chunk (tests forward_exactly_once)
        tx_r.push_chunk(chunk_r3);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I
                + 9 10"
            )
        );

        // push the 3rd left chunk
        // (tests that no change if there are still matches)
        tx_l.push_chunk(chunk_l3);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(chunk.into_chunk().unwrap(), StreamChunk::from_pretty("I I"));

        // push the 4th left chunk
        // (tests that insertion occurs when there are no more matches)
        tx_l.push_chunk(chunk_l4);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I
                + 1 4"
            )
        );
    }

    #[tokio::test]
    async fn test_streaming_hash_inner_join_with_barrier() {
        let chunk_l1 = StreamChunk::from_pretty(
            "  I I
             + 1 4
             + 2 5
             + 3 6",
        );
        let chunk_l2 = StreamChunk::from_pretty(
            "  I I
             + 6 8
             + 3 8",
        );
        let chunk_r1 = StreamChunk::from_pretty(
            "  I I
             + 2 7
             + 4 8
             + 6 9",
        );
        let chunk_r2 = StreamChunk::from_pretty(
            "  I  I
             + 3 10
             + 6 11",
        );
        let (mut tx_l, mut tx_r, mut hash_join) = create_executor::<{ JoinType::Inner }>(false);

        // push the init barrier for left and right
        tx_l.push_barrier(1, false);
        tx_r.push_barrier(1, false);
        hash_join.next().await.unwrap().unwrap();

        // push the 1st left chunk
        tx_l.push_chunk(chunk_l1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty("I I I I")
        );

        // push a barrier to left side
        tx_l.push_barrier(2, false);

        // push the 2nd left chunk
        tx_l.push_chunk(chunk_l2);

        // join the first right chunk
        tx_r.push_chunk(chunk_r1);

        // Consume stream chunk
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I I I
                + 2 5 2 7"
            )
        );

        // push a barrier to right side
        tx_r.push_barrier(2, false);

        // get the aligned barrier here
        let expected_epoch = Epoch::new_test_epoch(2);
        assert!(matches!(
            hash_join.next().await.unwrap().unwrap(),
            Message::Barrier(Barrier {
                epoch,
                mutation: None,
                ..
            }) if epoch == expected_epoch
        ));

        // join the 2nd left chunk
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I I I
                + 6 8 6 9"
            )
        );

        // push the 2nd right chunk
        tx_r.push_chunk(chunk_r2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I I I
                + 3 6 3 10
                + 3 8 3 10
                + 6 8 6 11"
            )
        );
    }

    #[tokio::test]
    async fn test_streaming_hash_inner_join_with_null_and_barrier() {
        let chunk_l1 = StreamChunk::from_pretty(
            "  I I
             + 1 4
             + 2 .
             + 3 .",
        );
        let chunk_l2 = StreamChunk::from_pretty(
            "  I I
             + 6 .
             + 3 8",
        );
        let chunk_r1 = StreamChunk::from_pretty(
            "  I I
             + 2 7
             + 4 8
             + 6 9",
        );
        let chunk_r2 = StreamChunk::from_pretty(
            "  I  I
             + 3 10
             + 6 11",
        );
        let (mut tx_l, mut tx_r, mut hash_join) = create_executor::<{ JoinType::Inner }>(false);

        // push the init barrier for left and right
        tx_l.push_barrier(1, false);
        tx_r.push_barrier(1, false);
        hash_join.next().await.unwrap().unwrap();

        // push the 1st left chunk
        tx_l.push_chunk(chunk_l1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty("I I I I")
        );

        // push a barrier to left side
        tx_l.push_barrier(2, false);

        // push the 2nd left chunk
        tx_l.push_chunk(chunk_l2);

        // join the first right chunk
        tx_r.push_chunk(chunk_r1);

        // Consume stream chunk
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I I I
                + 2 . 2 7"
            )
        );

        // push a barrier to right side
        tx_r.push_barrier(2, false);

        // get the aligned barrier here
        let expected_epoch = Epoch::new_test_epoch(2);
        assert!(matches!(
            hash_join.next().await.unwrap().unwrap(),
            Message::Barrier(Barrier {
                epoch,
                mutation: None,
                ..
            }) if epoch == expected_epoch
        ));

        // join the 2nd left chunk
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I I I
                + 6 . 6 9"
            )
        );

        // push the 2nd right chunk
        tx_r.push_chunk(chunk_r2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I I I
                + 3 . 3 10
                + 3 8 3 10
                + 6 . 6 11"
            )
        );
    }

    #[tokio::test]
    async fn test_streaming_hash_left_join() {
        let chunk_l1 = StreamChunk::from_pretty(
            "  I I
             + 1 4
             + 2 5
             + 3 6",
        );
        let chunk_l2 = StreamChunk::from_pretty(
            "  I I
             + 3 8
             - 3 8",
        );
        let chunk_r1 = StreamChunk::from_pretty(
            "  I I
             + 2 7
             + 4 8
             + 6 9",
        );
        let chunk_r2 = StreamChunk::from_pretty(
            "  I  I
             + 3 10
             + 6 11",
        );
        let (mut tx_l, mut tx_r, mut hash_join) = create_executor::<{ JoinType::LeftOuter }>(false);

        // push the init barrier for left and right
        tx_l.push_barrier(1, false);
        tx_r.push_barrier(1, false);
        hash_join.next().await.unwrap().unwrap();

        // push the 1st left chunk
        tx_l.push_chunk(chunk_l1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I I I
                + 1 4 . .
                + 2 5 . .
                + 3 6 . ."
            )
        );

        // push the 2nd left chunk
        tx_l.push_chunk(chunk_l2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I I I
                + 3 8 . .
                - 3 8 . ."
            )
        );

        // push the 1st right chunk
        tx_r.push_chunk(chunk_r1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                "  I I I I
                U- 2 5 . .
                U+ 2 5 2 7"
            )
        );

        // push the 2nd right chunk
        tx_r.push_chunk(chunk_r2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                "  I I I I
                U- 3 6 . .
                U+ 3 6 3 10"
            )
        );
    }

    #[tokio::test]
    async fn test_streaming_hash_right_join() {
        let chunk_l1 = StreamChunk::from_pretty(
            "  I I
             + 1 4
             + 2 5
             + 3 6",
        );
        let chunk_l2 = StreamChunk::from_pretty(
            "  I I
             + 3 8
             - 3 8",
        );
        let chunk_r1 = StreamChunk::from_pretty(
            "  I I
             + 2 7
             + 4 8
             + 6 9",
        );
        let chunk_r2 = StreamChunk::from_pretty(
            "  I  I
             + 5 10
             - 5 10",
        );
        let (mut tx_l, mut tx_r, mut hash_join) =
            create_executor::<{ JoinType::RightOuter }>(false);

        // push the init barrier for left and right
        tx_l.push_barrier(1, false);
        tx_r.push_barrier(1, false);
        hash_join.next().await.unwrap().unwrap();

        // push the 1st left chunk
        tx_l.push_chunk(chunk_l1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty("I I I I")
        );

        // push the 2nd left chunk
        tx_l.push_chunk(chunk_l2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty("I I I I")
        );

        // push the 1st right chunk
        tx_r.push_chunk(chunk_r1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I I I
                + 2 5 2 7
                + . . 4 8
                + . . 6 9"
            )
        );

        // push the 2nd right chunk
        tx_r.push_chunk(chunk_r2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I I I
                + . . 5 10
                - . . 5 10"
            )
        );
    }

    #[tokio::test]
    async fn test_streaming_hash_left_join_append_only() {
        let chunk_l1 = StreamChunk::from_pretty(
            "  I I I
             + 1 4 1
             + 2 5 2
             + 3 6 3",
        );
        let chunk_l2 = StreamChunk::from_pretty(
            "  I I I
             + 4 9 4
             + 5 10 5",
        );
        let chunk_r1 = StreamChunk::from_pretty(
            "  I I I
             + 2 5 1
             + 4 9 2
             + 6 9 3",
        );
        let chunk_r2 = StreamChunk::from_pretty(
            "  I I I
             + 1 4 4
             + 3 6 5",
        );

        let (mut tx_l, mut tx_r, mut hash_join) =
            create_append_only_executor::<{ JoinType::LeftOuter }>(false);

        // push the init barrier for left and right
        tx_l.push_barrier(1, false);
        tx_r.push_barrier(1, false);
        hash_join.next().await.unwrap().unwrap();

        // push the 1st left chunk
        tx_l.push_chunk(chunk_l1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I I I I I
                + 1 4 1 . . .
                + 2 5 2 . . .
                + 3 6 3 . . ."
            )
        );

        // push the 2nd left chunk
        tx_l.push_chunk(chunk_l2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I I I I I
                + 4 9 4 . . .
                + 5 10 5 . . ."
            )
        );

        // push the 1st right chunk
        tx_r.push_chunk(chunk_r1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                "  I I I I I I
                U- 2 5 2 . . .
                U+ 2 5 2 2 5 1
                U- 4 9 4 . . .
                U+ 4 9 4 4 9 2"
            )
        );

        // push the 2nd right chunk
        tx_r.push_chunk(chunk_r2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                "  I I I I I I
                U- 1 4 1 . . .
                U+ 1 4 1 1 4 4
                U- 3 6 3 . . .
                U+ 3 6 3 3 6 5"
            )
        );
    }

    #[tokio::test]
    async fn test_streaming_hash_right_join_append_only() {
        let chunk_l1 = StreamChunk::from_pretty(
            "  I I I
             + 1 4 1
             + 2 5 2
             + 3 6 3",
        );
        let chunk_l2 = StreamChunk::from_pretty(
            "  I I I
             + 4 9 4
             + 5 10 5",
        );
        let chunk_r1 = StreamChunk::from_pretty(
            "  I I I
             + 2 5 1
             + 4 9 2
             + 6 9 3",
        );
        let chunk_r2 = StreamChunk::from_pretty(
            "  I I I
             + 1 4 4
             + 3 6 5
             + 7 7 6",
        );

        let (mut tx_l, mut tx_r, mut hash_join) =
            create_append_only_executor::<{ JoinType::RightOuter }>(false);

        // push the init barrier for left and right
        tx_l.push_barrier(1, false);
        tx_r.push_barrier(1, false);
        hash_join.next().await.unwrap().unwrap();

        // push the 1st left chunk
        tx_l.push_chunk(chunk_l1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty("I I I I I I")
        );

        // push the 2nd left chunk
        tx_l.push_chunk(chunk_l2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty("I I I I I I")
        );

        // push the 1st right chunk
        tx_r.push_chunk(chunk_r1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                "  I I I I I I
                + 2 5 2 2 5 1
                + 4 9 4 4 9 2
                + . . . 6 9 3"
            )
        );

        // push the 2nd right chunk
        tx_r.push_chunk(chunk_r2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                "  I I I I I I
                + 1 4 1 1 4 4
                + 3 6 3 3 6 5
                + . . . 7 7 6"
            )
        );
    }

    #[tokio::test]
    async fn test_streaming_hash_full_outer_join() {
        let chunk_l1 = StreamChunk::from_pretty(
            "  I I
             + 1 4
             + 2 5
             + 3 6",
        );
        let chunk_l2 = StreamChunk::from_pretty(
            "  I I
             + 3 8
             - 3 8",
        );
        let chunk_r1 = StreamChunk::from_pretty(
            "  I I
             + 2 7
             + 4 8
             + 6 9",
        );
        let chunk_r2 = StreamChunk::from_pretty(
            "  I  I
             + 5 10
             - 5 10",
        );
        let (mut tx_l, mut tx_r, mut hash_join) = create_executor::<{ JoinType::FullOuter }>(false);

        // push the init barrier for left and right
        tx_l.push_barrier(1, false);
        tx_r.push_barrier(1, false);
        hash_join.next().await.unwrap().unwrap();

        // push the 1st left chunk
        tx_l.push_chunk(chunk_l1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I I I
                + 1 4 . .
                + 2 5 . .
                + 3 6 . ."
            )
        );

        // push the 2nd left chunk
        tx_l.push_chunk(chunk_l2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I I I
                + 3 8 . .
                - 3 8 . ."
            )
        );

        // push the 1st right chunk
        tx_r.push_chunk(chunk_r1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                "  I I I I
                U- 2 5 . .
                U+ 2 5 2 7
                +  . . 4 8
                +  . . 6 9"
            )
        );

        // push the 2nd right chunk
        tx_r.push_chunk(chunk_r2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I I I
                + . . 5 10
                - . . 5 10"
            )
        );
    }

    #[tokio::test]
    async fn test_streaming_hash_full_outer_join_with_nonequi_condition() {
        let chunk_l1 = StreamChunk::from_pretty(
            "  I I
             + 1 4
             + 2 5
             + 3 6
             + 3 7",
        );
        let chunk_l2 = StreamChunk::from_pretty(
            "  I I
             + 3 8
             - 3 8
             - 1 4", // delete row to cause an empty JoinHashEntry
        );
        let chunk_r1 = StreamChunk::from_pretty(
            "  I I
             + 2 6
             + 4 8
             + 3 4",
        );
        let chunk_r2 = StreamChunk::from_pretty(
            "  I  I
             + 5 10
             - 5 10
             + 1 2",
        );
        let (mut tx_l, mut tx_r, mut hash_join) = create_executor::<{ JoinType::FullOuter }>(true);

        // push the init barrier for left and right
        tx_l.push_barrier(1, false);
        tx_r.push_barrier(1, false);
        hash_join.next().await.unwrap().unwrap();

        // push the 1st left chunk
        tx_l.push_chunk(chunk_l1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I I I
                + 1 4 . .
                + 2 5 . .
                + 3 6 . .
                + 3 7 . ."
            )
        );

        // push the 2nd left chunk
        tx_l.push_chunk(chunk_l2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I I I
                + 3 8 . .
                - 3 8 . .
                - 1 4 . ."
            )
        );

        // push the 1st right chunk
        tx_r.push_chunk(chunk_r1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                "  I I I I
                U- 2 5 . .
                U+ 2 5 2 6
                +  . . 4 8
                +  . . 3 4" /* regression test (#2420): 3 4 should be forwarded only once
                             * despite matching on eq join on 2
                             * entries */
            )
        );

        // push the 2nd right chunk
        tx_r.push_chunk(chunk_r2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I I I
                + . . 5 10
                - . . 5 10
                + . . 1 2" /* regression test (#2420): 1 2 forwarded even if matches on an empty
                            * join entry */
            )
        );
    }

    #[tokio::test]
    async fn test_streaming_hash_inner_join_with_nonequi_condition() {
        let chunk_l1 = StreamChunk::from_pretty(
            "  I I
             + 1 4
             + 2 10
             + 3 6",
        );
        let chunk_l2 = StreamChunk::from_pretty(
            "  I I
             + 3 8
             - 3 8",
        );
        let chunk_r1 = StreamChunk::from_pretty(
            "  I I
             + 2 7
             + 4 8
             + 6 9",
        );
        let chunk_r2 = StreamChunk::from_pretty(
            "  I  I
             + 3 10
             + 6 11",
        );
        let (mut tx_l, mut tx_r, mut hash_join) = create_executor::<{ JoinType::Inner }>(true);

        // push the init barrier for left and right
        tx_l.push_barrier(1, false);
        tx_r.push_barrier(1, false);
        hash_join.next().await.unwrap().unwrap();

        // push the 1st left chunk
        tx_l.push_chunk(chunk_l1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty("I I I I")
        );

        // push the 2nd left chunk
        tx_l.push_chunk(chunk_l2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty("I I I I")
        );

        // push the 1st right chunk
        tx_r.push_chunk(chunk_r1);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty("I I I I")
        );

        // push the 2nd right chunk
        tx_r.push_chunk(chunk_r2);
        let chunk = hash_join.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " I I I I
                + 3 6 3 10"
            )
        );
    }
}
