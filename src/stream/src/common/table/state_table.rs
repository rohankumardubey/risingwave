// Copyright 2023 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::ops::Bound;
use std::ops::Bound::*;
use std::sync::Arc;

use async_stack_trace::StackTrace;
use bytes::{BufMut, Bytes, BytesMut};
use futures::{Stream, StreamExt};
use itertools::{izip, Itertools};
use risingwave_common::array::{Op, StreamChunk, Vis};
use risingwave_common::buffer::Bitmap;
use risingwave_common::catalog::{get_dist_key_in_pk_indices, ColumnDesc, TableId, TableOption};
use risingwave_common::hash::VirtualNode;
use risingwave_common::row::{self, CompactedRow, OwnedRow, Row, RowDeserializer, RowExt};
use risingwave_common::types::ScalarImpl;
use risingwave_common::util::epoch::EpochPair;
use risingwave_common::util::ordered::OrderedRowSerde;
use risingwave_common::util::sort_util::OrderType;
use risingwave_hummock_sdk::key::{
    end_bound_of_prefix, prefixed_range, range_of_prefix, start_bound_of_excluded_prefix,
};
use risingwave_pb::catalog::Table;
use risingwave_storage::error::StorageError;
use risingwave_storage::mem_table::MemTableError;
use risingwave_storage::row_serde::row_serde_util::{
    deserialize_pk_with_vnode, serialize_pk, serialize_pk_with_vnode,
};
use risingwave_storage::store::{LocalStateStore, ReadOptions, StateStoreIterItemStream};
use risingwave_storage::table::{compute_chunk_vnode, compute_vnode, Distribution};
use risingwave_storage::StateStore;
use tracing::trace;

use crate::executor::{StreamExecutorError, StreamExecutorResult};

/// This num is arbitrary and we may want to improve this choice in the future.
const STATE_CLEANING_PERIOD_EPOCH: usize = 5;

/// `StateTable` is the interface accessing relational data in KV(`StateStore`) with
/// row-based encoding.
#[derive(Clone)]
pub struct StateTable<S: StateStore> {
    /// Id for this table.
    table_id: TableId,

    /// State store backend.
    local_store: S::Local,

    /// Used for serializing and deserializing the primary key.
    pk_serde: OrderedRowSerde,

    /// Row deserializer with value encoding
    row_deserializer: RowDeserializer,

    /// Indices of primary key.
    /// Note that the index is based on the all columns of the table, instead of the output ones.
    // FIXME: revisit constructions and usages.
    pk_indices: Vec<usize>,

    /// Indices of distribution key for computing vnode.
    /// Note that the index is based on the all columns of the table, instead of the output ones.
    // FIXME: revisit constructions and usages.
    dist_key_indices: Vec<usize>,

    /// Indices of distribution key for computing vnode.
    /// Note that the index is based on the primary key columns by `pk_indices`.
    dist_key_in_pk_indices: Vec<usize>,

    prefix_hint_len: usize,

    /// Virtual nodes that the table is partitioned into.
    ///
    /// Only the rows whose vnode of the primary key is in this set will be visible to the
    /// executor. The table will also check whether the written rows
    /// conform to this partition.
    vnodes: Arc<Bitmap>,

    /// Used for catalog table_properties
    table_option: TableOption,

    /// If true, sanity check is disabled on this table.
    disable_sanity_check: bool,

    /// An optional column index which is the vnode of each row computed by the table's consistent
    /// hash distribution.
    vnode_col_idx_in_pk: Option<usize>,

    value_indices: Option<Vec<usize>>,

    /// last watermark that is used to construct delete ranges in `ingest`.
    last_watermark: Option<ScalarImpl>,

    /// latest watermark
    cur_watermark: Option<ScalarImpl>,

    /// number of commits with watermark since the last time we did state cleaning by watermark.
    num_wmked_commits_since_last_clean: usize,
}

// initialize
impl<S: StateStore> StateTable<S> {
    /// Create state table from table catalog and store.
    pub async fn from_table_catalog(
        table_catalog: &Table,
        store: S,
        vnodes: Option<Arc<Bitmap>>,
    ) -> Self {
        Self::from_table_catalog_may_disable_sanity_check(table_catalog, store, vnodes, false).await
    }

    /// Create state table from table catalog and store with sanity check disabled.
    pub async fn from_table_catalog_no_sanity_check(
        table_catalog: &Table,
        store: S,
        vnodes: Option<Arc<Bitmap>>,
    ) -> Self {
        Self::from_table_catalog_may_disable_sanity_check(table_catalog, store, vnodes, true).await
    }

    /// Create state table from table catalog and store.
    async fn from_table_catalog_may_disable_sanity_check(
        table_catalog: &Table,
        store: S,
        vnodes: Option<Arc<Bitmap>>,
        disable_sanity_check: bool,
    ) -> Self {
        let table_id = TableId::new(table_catalog.id);
        let table_columns: Vec<ColumnDesc> = table_catalog
            .columns
            .iter()
            .map(|col| col.column_desc.as_ref().unwrap().into())
            .collect();
        let order_types: Vec<OrderType> = table_catalog
            .pk
            .iter()
            .map(|col_order| {
                OrderType::from_prost(
                    &risingwave_pb::plan_common::OrderType::from_i32(col_order.order_type).unwrap(),
                )
            })
            .collect();
        let dist_key_indices: Vec<usize> = table_catalog
            .distribution_key
            .iter()
            .map(|dist_index| *dist_index as usize)
            .collect();

        let pk_indices = table_catalog
            .pk
            .iter()
            .map(|col_order| col_order.index as usize)
            .collect_vec();

        let dist_key_in_pk_indices = get_dist_key_in_pk_indices(&dist_key_indices, &pk_indices);
        let local_state_store = store.new_local(table_id).await;

        let pk_data_types = pk_indices
            .iter()
            .map(|i| table_columns[*i].data_type.clone())
            .collect();
        let pk_serde = OrderedRowSerde::new(pk_data_types, order_types);

        let Distribution {
            dist_key_indices,
            vnodes,
        } = match vnodes {
            Some(vnodes) => Distribution {
                dist_key_indices,
                vnodes,
            },
            None => Distribution::fallback(),
        };
        let vnode_col_idx_in_pk =
            table_catalog
                .vnode_col_index
                .as_ref()
                .and_then(|vnode_col_idx| {
                    let vnode_col_idx = vnode_col_idx.index as usize;
                    pk_indices.iter().position(|&i| vnode_col_idx == i)
                });
        let input_value_indices = table_catalog
            .value_indices
            .iter()
            .map(|val| *val as usize)
            .collect_vec();

        let data_types = input_value_indices
            .iter()
            .map(|idx| table_columns[*idx].data_type.clone())
            .collect();

        let no_shuffle_value_indices = (0..table_columns.len()).collect_vec();

        // if value_indices is the no shuffle full columns and
        let value_indices = match input_value_indices.len() == table_columns.len()
            && input_value_indices == no_shuffle_value_indices
        {
            true => None,
            false => Some(input_value_indices),
        };
        let prefix_hint_len = table_catalog.read_prefix_len_hint as usize;
        Self {
            table_id,
            local_store: local_state_store,
            pk_serde,
            row_deserializer: RowDeserializer::new(data_types),
            pk_indices: pk_indices.to_vec(),
            dist_key_indices,
            dist_key_in_pk_indices,
            prefix_hint_len,
            vnodes,
            table_option: TableOption::build_table_option(table_catalog.get_properties()),
            disable_sanity_check,
            vnode_col_idx_in_pk,
            value_indices,
            last_watermark: None,
            cur_watermark: None,
            num_wmked_commits_since_last_clean: 0,
        }
    }

    /// Create a state table without distribution, used for unit tests.
    pub async fn new_without_distribution(
        store: S,
        table_id: TableId,
        columns: Vec<ColumnDesc>,
        order_types: Vec<OrderType>,
        pk_indices: Vec<usize>,
    ) -> Self {
        Self::new_with_distribution(
            store,
            table_id,
            columns,
            order_types,
            pk_indices,
            Distribution::fallback(),
            None,
        )
        .await
    }

    /// Create a state table without distribution, used for unit tests.
    pub async fn new_without_distribution_no_sanity_check(
        store: S,
        table_id: TableId,
        columns: Vec<ColumnDesc>,
        order_types: Vec<OrderType>,
        pk_indices: Vec<usize>,
    ) -> Self {
        Self::new_with_distribution_may_disable_sanity_check(
            store,
            table_id,
            columns,
            order_types,
            pk_indices,
            Distribution::fallback(),
            None,
            true,
        )
        .await
    }

    /// Create a state table with given `value_indices`, used for unit tests.
    pub async fn new_without_distribution_partial(
        store: S,
        table_id: TableId,
        columns: Vec<ColumnDesc>,
        order_types: Vec<OrderType>,
        pk_indices: Vec<usize>,
        value_indices: Vec<usize>,
    ) -> Self {
        Self::new_with_distribution(
            store,
            table_id,
            columns,
            order_types,
            pk_indices,
            Distribution::fallback(),
            Some(value_indices),
        )
        .await
    }

    /// Create a state table with distribution specified with `distribution`. Should use
    /// `Distribution::fallback()` for tests.
    pub async fn new_with_distribution(
        store: S,
        table_id: TableId,
        table_columns: Vec<ColumnDesc>,
        order_types: Vec<OrderType>,
        pk_indices: Vec<usize>,
        distribution: Distribution,
        value_indices: Option<Vec<usize>>,
    ) -> Self {
        Self::new_with_distribution_may_disable_sanity_check(
            store,
            table_id,
            table_columns,
            order_types,
            pk_indices,
            distribution,
            value_indices,
            false,
        )
        .await
    }

    pub async fn new_with_distribution_no_sanity_check(
        store: S,
        table_id: TableId,
        table_columns: Vec<ColumnDesc>,
        order_types: Vec<OrderType>,
        pk_indices: Vec<usize>,
        distribution: Distribution,
        value_indices: Option<Vec<usize>>,
    ) -> Self {
        Self::new_with_distribution_may_disable_sanity_check(
            store,
            table_id,
            table_columns,
            order_types,
            pk_indices,
            distribution,
            value_indices,
            true,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn new_with_distribution_may_disable_sanity_check(
        store: S,
        table_id: TableId,
        table_columns: Vec<ColumnDesc>,
        order_types: Vec<OrderType>,
        pk_indices: Vec<usize>,
        Distribution {
            dist_key_indices,
            vnodes,
        }: Distribution,
        value_indices: Option<Vec<usize>>,
        disable_sanity_check: bool,
    ) -> Self {
        let local_state_store = store.new_local(table_id).await;

        let pk_data_types = pk_indices
            .iter()
            .map(|i| table_columns[*i].data_type.clone())
            .collect();
        let pk_serde = OrderedRowSerde::new(pk_data_types, order_types);

        let data_types = match &value_indices {
            Some(value_indices) => value_indices
                .iter()
                .map(|idx| table_columns[*idx].data_type.clone())
                .collect(),
            None => table_columns.iter().map(|c| c.data_type.clone()).collect(),
        };
        let dist_key_in_pk_indices = get_dist_key_in_pk_indices(&dist_key_indices, &pk_indices);
        Self {
            table_id,
            local_store: local_state_store,
            pk_serde,
            row_deserializer: RowDeserializer::new(data_types),
            pk_indices,
            dist_key_indices,
            dist_key_in_pk_indices,
            prefix_hint_len: 0,
            vnodes,
            table_option: Default::default(),
            disable_sanity_check,
            vnode_col_idx_in_pk: None,
            value_indices,
            last_watermark: None,
            cur_watermark: None,
            num_wmked_commits_since_last_clean: 0,
        }
    }

    fn table_id(&self) -> TableId {
        self.table_id
    }

    /// get the newest epoch of the state store and panic if the `init_epoch()` has never be called
    pub fn init_epoch(&mut self, epoch: EpochPair) {
        self.local_store.init(epoch.curr)
    }

    /// get the newest epoch of the state store and panic if the `init_epoch()` has never be called
    pub fn epoch(&self) -> u64 {
        self.local_store.epoch()
    }

    /// Get the vnode value with given (prefix of) primary key
    fn compute_prefix_vnode(&self, pk_prefix: impl Row) -> VirtualNode {
        let prefix_len = pk_prefix.len();
        if let Some(vnode_col_idx_in_pk) = self.vnode_col_idx_in_pk {
            let vnode = pk_prefix.datum_at(vnode_col_idx_in_pk).unwrap();
            VirtualNode::from_scalar(vnode.into_int16())
        } else {
            // For streaming, the given prefix must be enough to calculate the vnode
            assert!(self.dist_key_in_pk_indices.iter().all(|&d| d < prefix_len));
            compute_vnode(pk_prefix, &self.dist_key_in_pk_indices, &self.vnodes)
        }
    }

    /// Get the vnode value of the given row
    pub fn compute_vnode(&self, row: impl Row) -> VirtualNode {
        compute_vnode(row, &self.dist_key_indices, &self.vnodes)
    }

    // TODO: remove, should not be exposed to user
    pub fn pk_indices(&self) -> &[usize] {
        &self.pk_indices
    }

    pub fn pk_serde(&self) -> &OrderedRowSerde {
        &self.pk_serde
    }

    pub fn dist_key_indices(&self) -> &[usize] {
        &self.dist_key_indices
    }

    pub fn vnodes(&self) -> &Arc<Bitmap> {
        &self.vnodes
    }

    pub fn value_indices(&self) -> &Option<Vec<usize>> {
        &self.value_indices
    }

    pub fn is_dirty(&self) -> bool {
        self.local_store.is_dirty()
    }

    pub fn vnode_bitmap(&self) -> &Bitmap {
        &self.vnodes
    }
}

const ENABLE_SANITY_CHECK: bool = cfg!(debug_assertions);

// point get
impl<S: StateStore> StateTable<S> {
    /// Get a single row from state table.
    pub async fn get_row(&self, pk: impl Row) -> StreamExecutorResult<Option<OwnedRow>> {
        let compacted_row: Option<CompactedRow> = self.get_compacted_row(pk).await?;
        match compacted_row {
            Some(compacted_row) => {
                let row = self
                    .row_deserializer
                    .deserialize(compacted_row.row.as_ref())?;
                Ok(Some(row))
            }
            None => Ok(None),
        }
    }

    /// Get a compacted row from state table.
    pub async fn get_compacted_row(
        &self,
        pk: impl Row,
    ) -> StreamExecutorResult<Option<CompactedRow>> {
        let serialized_pk =
            serialize_pk_with_vnode(&pk, &self.pk_serde, self.compute_prefix_vnode(&pk));
        assert!(pk.len() <= self.pk_indices.len());

        if self.prefix_hint_len != 0 {
            debug_assert_eq!(self.prefix_hint_len, pk.len());
        }

        let read_options = ReadOptions {
            prefix_hint: None,
            check_bloom_filter: self.prefix_hint_len != 0 && self.prefix_hint_len == pk.len(),
            retention_seconds: self.table_option.retention_seconds,
            table_id: self.table_id,
            ignore_range_tombstone: false,
            read_version_from_backup: false,
        };
        if let Some(storage_row_bytes) = self.local_store.get(&serialized_pk, read_options).await? {
            Ok(Some(CompactedRow {
                row: storage_row_bytes,
            }))
        } else {
            Ok(None)
        }
    }

    /// Update the vnode bitmap of the state table, returns the previous vnode bitmap.
    #[must_use = "the executor should decide whether to manipulate the cache based on the previous vnode bitmap"]
    pub fn update_vnode_bitmap(&mut self, new_vnodes: Arc<Bitmap>) -> Arc<Bitmap> {
        assert!(
            !self.is_dirty(),
            "vnode bitmap should only be updated when state table is clean"
        );
        if self.dist_key_indices.is_empty() {
            assert_eq!(
                new_vnodes, self.vnodes,
                "should not update vnode bitmap for singleton table"
            );
        }
        assert_eq!(self.vnodes.len(), new_vnodes.len());

        self.cur_watermark = None;
        self.last_watermark = None;

        std::mem::replace(&mut self.vnodes, new_vnodes)
    }
}
// write
impl<S: StateStore> StateTable<S> {
    #[expect(clippy::boxed_local)]
    fn handle_mem_table_error(&self, e: StorageError) {
        let e = match e {
            StorageError::MemTable(e) => e,
            _ => unreachable!("should only get memtable error"),
        };
        match *e {
            MemTableError::Conflict { key, prev, new } => {
                let (vnode, key) = deserialize_pk_with_vnode(&key, &self.pk_serde).unwrap();
                panic!(
                    "mem-table operation conflicts! table_id: {}, vnode: {}, key: {:?}, prev: {}, new: {}",
                    self.table_id(),
                    vnode,
                    &key,
                    prev.debug_fmt(&self.row_deserializer),
                    new.debug_fmt(&self.row_deserializer),
                )
            }
        }
    }

    fn serialize_value(&self, value: impl Row) -> Bytes {
        if let Some(value_indices) = self.value_indices.as_ref() {
            value.project(value_indices).value_serialize_bytes()
        } else {
            value.value_serialize_bytes()
        }
    }

    fn insert_inner(&mut self, key_bytes: Bytes, value_bytes: Bytes) {
        self.local_store
            .insert(key_bytes, value_bytes, None)
            .unwrap_or_else(|e| self.handle_mem_table_error(e));
    }

    fn delete_inner(&mut self, key_bytes: Bytes, value_bytes: Bytes) {
        self.local_store
            .delete(key_bytes, value_bytes)
            .unwrap_or_else(|e| self.handle_mem_table_error(e));
    }

    fn update_inner(&mut self, key_bytes: Bytes, old_value_bytes: Bytes, new_value_bytes: Bytes) {
        self.local_store
            .insert(key_bytes, new_value_bytes, Some(old_value_bytes))
            .unwrap_or_else(|e| self.handle_mem_table_error(e));
    }

    /// Insert a row into state table. Must provide a full row corresponding to the column desc of
    /// the table.
    pub fn insert(&mut self, value: impl Row) {
        let pk = (&value).project(self.pk_indices());

        let key_bytes = serialize_pk_with_vnode(pk, &self.pk_serde, self.compute_prefix_vnode(pk));
        let value_bytes = self.serialize_value(value);
        self.insert_inner(key_bytes, value_bytes);
    }

    /// Delete a row from state table. Must provide a full row of old value corresponding to the
    /// column desc of the table.
    pub fn delete(&mut self, old_value: impl Row) {
        let pk = (&old_value).project(self.pk_indices());

        let key_bytes = serialize_pk_with_vnode(pk, &self.pk_serde, self.compute_prefix_vnode(pk));
        let value_bytes = self.serialize_value(old_value);
        self.delete_inner(key_bytes, value_bytes);
    }

    /// Update a row. The old and new value should have the same pk.
    pub fn update(&mut self, old_value: impl Row, new_value: impl Row) {
        let old_pk = (&old_value).project(self.pk_indices());
        let new_pk = (&new_value).project(self.pk_indices());
        debug_assert!(
            Row::eq(&old_pk, new_pk),
            "pk should not change: {old_pk:?} vs {new_pk:?}",
        );

        let new_key_bytes =
            serialize_pk_with_vnode(new_pk, &self.pk_serde, self.compute_prefix_vnode(new_pk));
        let old_value_bytes = self.serialize_value(old_value);
        let new_value_bytes = self.serialize_value(new_value);

        self.update_inner(new_key_bytes, old_value_bytes, new_value_bytes);
    }

    /// Write batch with a `StreamChunk` which should have the same schema with the table.
    // allow(izip, which use zip instead of zip_eq)
    #[allow(clippy::disallowed_methods)]
    pub fn write_chunk(&mut self, chunk: StreamChunk) {
        let (chunk, op) = chunk.into_parts();

        let vnodes = compute_chunk_vnode(&chunk, &self.dist_key_indices, &self.vnodes);

        let value_chunk = if let Some(ref value_indices) = self.value_indices {
            chunk.clone().reorder_columns(value_indices)
        } else {
            chunk.clone()
        };
        let values = value_chunk.serialize();

        let key_chunk = chunk.reorder_columns(self.pk_indices());
        let vnode_and_pks = key_chunk
            .rows_with_holes()
            .zip_eq(vnodes.iter())
            .map(|(r, vnode)| {
                let mut buffer = BytesMut::new();
                buffer.put_slice(&vnode.to_be_bytes()[..]);
                if let Some(r) = r {
                    self.pk_serde.serialize(r, &mut buffer);
                }
                buffer.freeze()
            })
            .collect_vec();

        let (_, vis) = key_chunk.into_parts();
        match vis {
            Vis::Bitmap(vis) => {
                for ((op, key, value), vis) in izip!(op, vnode_and_pks, values).zip_eq(vis.iter()) {
                    if vis {
                        match op {
                            Op::Insert | Op::UpdateInsert => self.insert_inner(key, value),
                            Op::Delete | Op::UpdateDelete => self.delete_inner(key, value),
                        }
                    }
                }
            }
            Vis::Compact(_) => {
                for (op, key, value) in izip!(op, vnode_and_pks, values) {
                    match op {
                        Op::Insert | Op::UpdateInsert => self.insert_inner(key, value),
                        Op::Delete | Op::UpdateDelete => self.delete_inner(key, value),
                    }
                }
            }
        }
    }

    pub fn update_watermark(&mut self, watermark: ScalarImpl) {
        self.cur_watermark = Some(watermark);
    }

    pub async fn commit(&mut self, new_epoch: EpochPair) -> StreamExecutorResult<()> {
        assert_eq!(self.epoch(), new_epoch.prev);
        if self.cur_watermark.is_some() {
            self.num_wmked_commits_since_last_clean += 1;
        }
        Ok(self.seal_current_epoch(new_epoch.curr).await?)
    }

    /// used for unit test, and do not need to assert epoch.
    pub async fn commit_for_test(&mut self, new_epoch: EpochPair) -> StreamExecutorResult<()> {
        Ok(self.seal_current_epoch(new_epoch.curr).await?)
    }

    // TODO(st1page): maybe we should extract a pub struct to do it
    /// just specially used by those state table read-only and after the call the data
    /// in the epoch will be visible
    pub async fn commit_no_data_expected(&mut self, new_epoch: EpochPair) {
        assert_eq!(self.epoch(), new_epoch.prev);
        assert!(!self.is_dirty());
        self.local_store
            .seal_current_epoch(new_epoch.curr, Vec::new())
            .await
            .expect("should not get any storage error when no data");
    }

    /// Write to state store.
    async fn seal_current_epoch(&mut self, next_epoch: u64) -> StreamExecutorResult<()> {
        let watermark = self.cur_watermark.as_ref().and_then(|cur_watermark_ref| {
            self.num_wmked_commits_since_last_clean += 1;

            if self.num_wmked_commits_since_last_clean >= STATE_CLEANING_PERIOD_EPOCH {
                Some(cur_watermark_ref)
            } else {
                None
            }
        });

        let mut delete_ranges = Vec::new();

        let prefix_serializer = if self.pk_indices().is_empty() {
            None
        } else {
            Some(self.pk_serde.prefix(1))
        };
        let range_end_suffix = watermark.map(|watermark| {
            serialize_pk(
                row::once(Some(watermark.clone())),
                prefix_serializer.as_ref().unwrap(),
            )
        });
        //     for (pk, row_op) in buffer {
        //         if let Some(ref range_end) = range_end_suffix && &pk[VirtualNode::SIZE..] <
        // range_end.as_slice() {             continue;
        //         }
        //         match row_op {
        //             // Currently, some executors do not strictly comply with these semantics. As
        // a             // workaround you may call disable the check by initializing the
        // state store with             // `disable_sanity_check=true`.
        //             KeyOp::Insert(row) => {
        //                 if ENABLE_SANITY_CHECK && !self.disable_sanity_check {
        //                     self.do_insert_sanity_check(&pk, &row, epoch).await?;
        //                 }
        //                 write_batch.put(pk, StorageValue::new_put(row));
        //             }
        //             KeyOp::Delete(row) => {
        //                 if ENABLE_SANITY_CHECK && !self.disable_sanity_check {
        //                     self.do_delete_sanity_check(&pk, &row, epoch).await?;
        //                 }
        //                 write_batch.delete(pk);
        //             }
        //             KeyOp::Update((old_row, new_row)) => {
        //                 if ENABLE_SANITY_CHECK && !self.disable_sanity_check {
        //                     self.do_update_sanity_check(&pk, &old_row, &new_row, epoch)
        //                         .await?;
        //                 }
        //                 write_batch.put(pk, StorageValue::new_put(new_row));
        //             }
        //         }
        //     }
        if let Some(range_end_suffix) = range_end_suffix {
            let range_begin_suffix = if let Some(ref last_watermark) = self.last_watermark {
                serialize_pk(
                    row::once(Some(last_watermark.clone())),
                    prefix_serializer.as_ref().unwrap(),
                )
            } else {
                vec![]
            };
            for vnode in self.vnodes.iter_ones() {
                let mut range_begin = vnode.to_be_bytes().to_vec();
                let mut range_end = range_begin.clone();
                range_begin.extend(&range_begin_suffix);
                range_end.extend(&range_end_suffix);
                delete_ranges.push((Bytes::from(range_begin), Bytes::from(range_end)));
            }
        }
        self.local_store
            .seal_current_epoch(next_epoch, delete_ranges)
            .await?;
        if watermark.is_some() {
            self.last_watermark = self.cur_watermark.take();
            self.num_wmked_commits_since_last_clean = 0;
        }
        Ok(())
    }

    /// Make sure the key to insert should not exist in storage.
    async fn do_insert_sanity_check(
        &self,
        key: &[u8],
        value: &[u8],
    ) -> StreamExecutorResult<()> {
        let read_options = ReadOptions {
            prefix_hint: None,
            check_bloom_filter: false,
            retention_seconds: self.table_option.retention_seconds,
            table_id: self.table_id,
            ignore_range_tombstone: false,
            read_version_from_backup: false,
        };
        let stored_value = self.local_store.get(key, read_options).await?;

        if let Some(stored_value) = stored_value {
            let (vnode, key) = deserialize_pk_with_vnode(key, &self.pk_serde).unwrap();
            let in_storage = self.row_deserializer.deserialize(stored_value).unwrap();
            let to_write = self.row_deserializer.deserialize(value).unwrap();
            panic!(
                "overwrites an existing key!\ntable_id: {}, vnode: {}, key: {:?}\nvalue in storage: {:?}\nvalue to write: {:?}",
                self.table_id(),
                vnode,
                key,
                in_storage,
                to_write,
            );
        }
        Ok(())
    }

    /// Make sure that the key to delete should exist in storage and the value should be matched.
    async fn do_delete_sanity_check(
        &self,
        key: &[u8],
        old_row: &[u8],
    ) -> StreamExecutorResult<()> {
        let read_options = ReadOptions {
            prefix_hint: None,
            check_bloom_filter: false,
            retention_seconds: self.table_option.retention_seconds,
            table_id: self.table_id,
            ignore_range_tombstone: false,
            read_version_from_backup: false,
        };
        let stored_value = self.local_store.get(key, read_options).await?;

        if stored_value.is_none() || stored_value.as_ref().unwrap() != old_row {
            let (vnode, key) = deserialize_pk_with_vnode(key, &self.pk_serde).unwrap();
            let stored_row =
                stored_value.map(|bytes| self.row_deserializer.deserialize(bytes).unwrap());
            let to_delete = self.row_deserializer.deserialize(old_row).unwrap();
            panic!(
                "inconsistent delete!\ntable_id: {}, vnode: {}, key: {:?}\nstored value: {:?}\nexpected value: {:?}",
                self.table_id(),
                vnode,
                key,
                stored_row,
                to_delete,
            );
        }
        Ok(())
    }

    /// Make sure that the key to update should exist in storage and the value should be matched
    async fn do_update_sanity_check(
        &self,
        key: &[u8],
        old_row: &[u8],
        new_row: &[u8],
    ) -> StreamExecutorResult<()> {
        let read_options = ReadOptions {
            prefix_hint: None,
            ignore_range_tombstone: false,
            check_bloom_filter: false,
            retention_seconds: self.table_option.retention_seconds,
            table_id: self.table_id,
            read_version_from_backup: false,
        };
        let stored_value = self.local_store.get(key, read_options).await?;

        if stored_value.is_none() || stored_value.as_ref().unwrap() != old_row {
            let (vnode, key) = deserialize_pk_with_vnode(key, &self.pk_serde).unwrap();
            let expected_row = self.row_deserializer.deserialize(old_row).unwrap();
            let stored_row =
                stored_value.map(|bytes| self.row_deserializer.deserialize(bytes).unwrap());
            let new_row = self.row_deserializer.deserialize(new_row).unwrap();
            panic!(
                "inconsistent update!\ntable_id: {}, vnode: {}, key: {:?}\nstored value: {:?}\nexpected value: {:?}\nnew value: {:?}",
                self.table_id(),
                vnode,
                key,
                stored_row,
                expected_row,
                new_row,
            );
        }

        Ok(())
    }
}

fn get_second<T, U>(arg: StreamExecutorResult<(T, U)>) -> StreamExecutorResult<U> {
    arg.map(|x| x.1)
}

// Iterator functions
impl<S: StateStore> StateTable<S> {
    /// This function scans rows from the relational table.
    pub async fn iter(&self) -> StreamExecutorResult<RowStream<'_, S>> {
        self.iter_with_pk_prefix(row::empty()).await
    }

    /// This function scans rows from the relational table with specific `pk_prefix`.
    pub async fn iter_with_pk_prefix(
        &self,
        pk_prefix: impl Row,
    ) -> StreamExecutorResult<RowStream<'_, S>> {
        Ok(self.iter_key_and_val(pk_prefix).await?.map(get_second))
    }

    /// This function scans rows from the relational table with specific `pk_prefix`.
    async fn iter_with_pk_range_inner(
        &self,
        pk_range: &(Bound<impl Row>, Bound<impl Row>),
        // Optional vnode that returns an iterator only over the given range under that vnode.
        // For now, we require this parameter, and will panic. In the future, when `None`, we can
        // iterate over each vnode that the `StateTable` owns.
        vnode: VirtualNode,
    ) -> StreamExecutorResult<<S::Local as LocalStateStore>::IterStream<'_>> {
        let memcomparable_range = prefix_range_to_memcomparable(&self.pk_serde, pk_range);

        let memcomparable_range_with_vnode =
            prefixed_range(memcomparable_range, &vnode.to_be_bytes());

        // TODO: provide a trace of useful params.
        self.iter_inner(memcomparable_range_with_vnode, None)
            .await
            .map_err(StreamExecutorError::from)
    }

    pub async fn iter_with_pk_range(
        &self,
        pk_range: &(Bound<impl Row>, Bound<impl Row>),
        // Optional vnode that returns an iterator only over the given range under that vnode.
        // For now, we require this parameter, and will panic. In the future, when `None`, we can
        // iterate over each vnode that the `StateTable` owns.
        vnode: VirtualNode,
    ) -> StreamExecutorResult<RowStream<'_, S>> {
        Ok(self
            .iter_key_and_val_with_pk_range(pk_range, vnode)
            .await?
            .map(get_second))
    }

    pub async fn iter_key_and_val_with_pk_range(
        &self,
        pk_range: &(Bound<impl Row>, Bound<impl Row>),
        // Optional vnode that returns an iterator only over the given range under that vnode.
        // For now, we require this parameter, and will panic. In the future, when `None`, we can
        // iterate over each vnode that the `StateTable` owns.
        vnode: VirtualNode,
    ) -> StreamExecutorResult<RowStreamWithPk<'_, S>> {
        Ok(deserialize_row_stream(
            self.iter_with_pk_range_inner(pk_range, vnode).await?,
            self.row_deserializer.clone(),
        ))
    }

    /// This function scans rows from the relational table with specific `pk_prefix`, return both
    /// key and value.
    pub async fn iter_key_and_val(
        &self,
        pk_prefix: impl Row,
    ) -> StreamExecutorResult<RowStreamWithPk<'_, S>> {
        Ok(deserialize_row_stream(
            self.iter_with_pk_prefix_inner(pk_prefix).await?,
            self.row_deserializer.clone(),
        ))
    }

    async fn iter_with_pk_prefix_inner(
        &self,
        pk_prefix: impl Row,
    ) -> StreamExecutorResult<<S::Local as LocalStateStore>::IterStream<'_>> {
        let prefix_serializer = self.pk_serde.prefix(pk_prefix.len());
        let encoded_prefix = serialize_pk(&pk_prefix, &prefix_serializer);
        let encoded_key_range = range_of_prefix(&encoded_prefix);

        // We assume that all usages of iterating the state table only access a single vnode.
        // If this assertion fails, then something must be wrong with the operator implementation or
        // the distribution derivation from the optimizer.
        let vnode = self.compute_prefix_vnode(&pk_prefix).to_be_bytes();
        let encoded_key_range_with_vnode = prefixed_range(encoded_key_range, &vnode);

        // Construct prefix hint for prefix bloom filter.
        let pk_prefix_indices = &self.pk_indices[..pk_prefix.len()];
        if self.prefix_hint_len != 0 {
            debug_assert_eq!(self.prefix_hint_len, pk_prefix.len());
        }
        let prefix_hint = {
            if self.prefix_hint_len == 0 || self.prefix_hint_len > pk_prefix.len() {
                None
            } else {
                let encoded_prefix_len = self
                    .pk_serde
                    .deserialize_prefix_len(&encoded_prefix, self.prefix_hint_len)?;

                Some(encoded_prefix[..encoded_prefix_len].to_vec())
            }
        };

        trace!(
            table_id = ?self.table_id(),
            ?prefix_hint, ?encoded_key_range_with_vnode, ?pk_prefix,
            dist_key_indices = ?self.dist_key_indices, ?pk_prefix_indices,
            "storage_iter_with_prefix"
        );

        self.iter_inner(encoded_key_range_with_vnode, prefix_hint)
            .await
    }

    async fn iter_inner(
        &self,
        key_range: (Bound<Vec<u8>>, Bound<Vec<u8>>),
        prefix_hint: Option<Vec<u8>>,
    ) -> StreamExecutorResult<<S::Local as LocalStateStore>::IterStream<'_>> {
        let (l, r) = key_range.clone();
        let bytes_key_range = (l.map(Bytes::from), r.map(Bytes::from));

        let check_bloom_filter = prefix_hint.is_some();

        let read_options = ReadOptions {
            prefix_hint,
            check_bloom_filter,
            ignore_range_tombstone: false,
            retention_seconds: self.table_option.retention_seconds,
            table_id: self.table_id,
            read_version_from_backup: false,
        };

        Ok(self.local_store.iter(key_range, read_options).await?)
    }

    pub fn get_vnodes(&self) -> Arc<Bitmap> {
        self.vnodes.clone()
    }
}

pub type RowStream<'a, S: StateStore> = impl Stream<Item = StreamExecutorResult<OwnedRow>> + 'a;
pub type RowStreamWithPk<'a, S: StateStore> =
    impl Stream<Item = StreamExecutorResult<(Bytes, OwnedRow)>> + 'a;

fn deserialize_row_stream(
    stream: impl StateStoreIterItemStream,
    deserializer: RowDeserializer,
) -> impl Stream<Item = StreamExecutorResult<(Bytes, OwnedRow)>> {
    stream.map(move |result| {
        result
            .map_err(StreamExecutorError::from)
            .and_then(|(key, value)| {
                Ok(deserializer
                    .deserialize(value)
                    .map(move |row| (key.user_key.table_key.0, row))?)
            })
    })
}

pub fn prefix_range_to_memcomparable(
    pk_serde: &OrderedRowSerde,
    range: &(Bound<impl Row>, Bound<impl Row>),
) -> (Bound<Vec<u8>>, Bound<Vec<u8>>) {
    (
        to_memcomparable(pk_serde, &range.0, false),
        to_memcomparable(pk_serde, &range.1, true),
    )
}

fn to_memcomparable<R: Row>(
    pk_serde: &OrderedRowSerde,
    bound: &Bound<R>,
    is_upper: bool,
) -> Bound<Vec<u8>> {
    let serialize_pk_prefix = |pk_prefix: &R| {
        let prefix_serializer = pk_serde.prefix(pk_prefix.len());
        serialize_pk(pk_prefix, &prefix_serializer)
    };
    match bound {
        Unbounded => Unbounded,
        Included(r) => {
            let serialized = serialize_pk_prefix(r);
            if is_upper {
                end_bound_of_prefix(&serialized)
            } else {
                Included(serialized)
            }
        }
        Excluded(r) => {
            let serialized = serialize_pk_prefix(r);
            if !is_upper {
                // if lower
                start_bound_of_excluded_prefix(&serialized)
            } else {
                Excluded(serialized)
            }
        }
    }
}
