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

use anyhow::anyhow;
use futures::future::try_join_all;
use futures_async_stream::try_stream;
use itertools::Itertools;
use risingwave_common::array::column::Column;
use risingwave_common::array::{ArrayBuilder, DataChunk, Op, PrimitiveArrayBuilder, StreamChunk};
use risingwave_common::catalog::{Field, Schema, TableId};
use risingwave_common::error::{Result, RwError};
use risingwave_common::types::DataType;
use risingwave_expr::expr::{build_from_prost, BoxedExpression};
use risingwave_pb::batch_plan::plan_node::NodeBody;
use risingwave_source::dml_manager::DmlManagerRef;

use crate::error::BatchError;
use crate::executor::{
    BoxedDataChunkStream, BoxedExecutor, BoxedExecutorBuilder, Executor, ExecutorBuilder,
};
use crate::task::BatchTaskContext;
/// [`UpdateExecutor`] implements table updation with values from its child executor and given
/// expressions.
// TODO: multiple `UPDATE`s in a single epoch may cause problems. Need validation on materialize.
// TODO: concurrent `UPDATE` may cause problems. A scheduler might be required.
pub struct UpdateExecutor {
    /// Target table id.
    table_id: TableId,
    dml_manager: DmlManagerRef,
    child: BoxedExecutor,
    exprs: Vec<BoxedExpression>,
    schema: Schema,
    identity: String,
}

impl UpdateExecutor {
    pub fn new(
        table_id: TableId,
        dml_manager: DmlManagerRef,
        child: BoxedExecutor,
        exprs: Vec<BoxedExpression>,
        identity: String,
    ) -> Self {
        assert_eq!(
            child.schema().data_types(),
            exprs.iter().map(|e| e.return_type()).collect_vec(),
            "bad update schema"
        );

        Self {
            table_id,
            dml_manager,
            child,
            exprs,
            // TODO: support `RETURNING`
            schema: Schema {
                fields: vec![Field::unnamed(DataType::Int64)],
            },
            identity,
        }
    }
}

impl Executor for UpdateExecutor {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn identity(&self) -> &str {
        &self.identity
    }

    fn execute(self: Box<Self>) -> BoxedDataChunkStream {
        self.do_execute()
    }
}

impl UpdateExecutor {
    #[try_stream(boxed, ok = DataChunk, error = RwError)]
    async fn do_execute(mut self: Box<Self>) {
        let schema = self.child.schema().clone();
        let mut notifiers = Vec::new();

        #[for_await]
        for data_chunk in self.child.execute() {
            let data_chunk = data_chunk?.compact();
            let len = data_chunk.cardinality();

            let updated_data_chunk = {
                let columns: Vec<_> = self
                    .exprs
                    .iter_mut()
                    .map(|expr| expr.eval(&data_chunk).map(Column::new))
                    .try_collect()?;

                DataChunk::new(columns, len)
            };

            // Merge two data chunks into (U-, U+) pairs.
            // TODO: split chunks
            let mut builders = schema.create_array_builders(len * 2);
            for row in data_chunk
                .rows()
                .zip_eq(updated_data_chunk.rows())
                .flat_map(|(a, b)| [a, b])
            {
                for (datum_ref, builder) in row.values().zip_eq(builders.iter_mut()) {
                    builder.append_datum(datum_ref);
                }
            }
            let columns = builders.into_iter().map(|b| b.finish().into()).collect();

            let ops = [Op::UpdateDelete, Op::UpdateInsert]
                .into_iter()
                .cycle()
                .take(len * 2)
                .collect();

            let stream_chunk = StreamChunk::new(ops, columns, None);

            let notifier = self.dml_manager.write_chunk(&self.table_id, stream_chunk)?;
            notifiers.push(notifier);
        }

        // Wait for all chunks to be taken / written.
        let rows_updated = try_join_all(notifiers)
            .await
            .map_err(|_| {
                BatchError::Internal(anyhow!("failed to wait chunks to be written".to_owned(),))
            })?
            .into_iter()
            .sum::<usize>()
            / 2;

        // Create ret value
        {
            let mut array_builder = PrimitiveArrayBuilder::<i64>::new(1);
            array_builder.append(Some(rows_updated as i64));

            let array = array_builder.finish();
            let ret_chunk = DataChunk::new(vec![array.into()], 1);

            yield ret_chunk
        }
    }
}

#[async_trait::async_trait]
impl BoxedExecutorBuilder for UpdateExecutor {
    async fn new_boxed_executor<C: BatchTaskContext>(
        source: &ExecutorBuilder<'_, C>,
        inputs: Vec<BoxedExecutor>,
    ) -> Result<BoxedExecutor> {
        let [child]: [_; 1] = inputs.try_into().unwrap();

        let update_node = try_match_expand!(
            source.plan_node().get_node_body().unwrap(),
            NodeBody::Update
        )?;

        let table_id = TableId::new(update_node.table_id);

        let exprs: Vec<_> = update_node
            .get_exprs()
            .iter()
            .map(build_from_prost)
            .try_collect()?;

        Ok(Box::new(Self::new(
            table_id,
            source.context().dml_manager(),
            child,
            exprs,
            source.plan_node().get_identity().clone(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::StreamExt;
    use risingwave_common::array::Array;
    use risingwave_common::catalog::{schema_test_utils, ColumnDesc, ColumnId};
    use risingwave_common::test_prelude::DataChunkTestExt;
    use risingwave_expr::expr::InputRefExpression;
    use risingwave_source::dml_manager::DmlManager;

    use super::*;
    use crate::executor::test_utils::MockExecutor;
    use crate::*;

    #[tokio::test]
    async fn test_update_executor() -> Result<()> {
        let dml_manager = Arc::new(DmlManager::default());

        // Schema for mock executor.
        let schema = schema_test_utils::ii();
        let mut mock_executor = MockExecutor::new(schema.clone());

        // Schema of the table
        let schema = schema_test_utils::ii();

        mock_executor.add(DataChunk::from_pretty(
            "i  i
             1  2
             3  4
             5  6
             7  8
             9 10",
        ));

        // Update expressions, will swap two columns.
        let exprs = vec![
            Box::new(InputRefExpression::new(DataType::Int32, 1)) as BoxedExpression,
            Box::new(InputRefExpression::new(DataType::Int32, 0)),
        ];

        // Create the table.
        let table_id = TableId::new(0);

        // Create reader
        let column_descs = schema
            .fields
            .iter()
            .enumerate()
            .map(|(i, field)| ColumnDesc::unnamed(ColumnId::new(i as _), field.data_type.clone()))
            .collect_vec();
        // We must create a variable to hold this `Arc<TableSource>` here, or it will be dropped due
        // to the `Weak` reference in `DmlManager`.
        let reader = dml_manager
            .register_reader(table_id, &column_descs)
            .unwrap();
        let mut reader = reader.stream_reader_v2().into_stream_v2();

        // Update
        let update_executor = Box::new(UpdateExecutor::new(
            table_id,
            dml_manager,
            Box::new(mock_executor),
            exprs,
            "UpdateExecutor".to_string(),
        ));

        let handle = tokio::spawn(async move {
            let fields = &update_executor.schema().fields;
            assert_eq!(fields[0].data_type, DataType::Int64);

            let mut stream = update_executor.execute();
            let result = stream.next().await.unwrap().unwrap();

            assert_eq!(
                result
                    .column_at(0)
                    .array()
                    .as_int64()
                    .iter()
                    .collect::<Vec<_>>(),
                vec![Some(5)] // updated rows
            );
        });

        // Read
        let chunk = reader.next().await.unwrap()?;

        assert_eq!(
            chunk.ops().chunks(2).collect_vec(),
            vec![&[Op::UpdateDelete, Op::UpdateInsert]; 5]
        );

        assert_eq!(
            chunk.columns()[0]
                .array()
                .as_int32()
                .iter()
                .collect::<Vec<_>>(),
            (1..=5)
                .flat_map(|i| [i * 2 - 1, i * 2]) // -1, +2, -3, +4, ...
                .map(Some)
                .collect_vec()
        );

        assert_eq!(
            chunk.columns()[1]
                .array()
                .as_int32()
                .iter()
                .collect::<Vec<_>>(),
            (1..=5)
                .flat_map(|i| [i * 2, i * 2 - 1]) // -2, +1, -4, +3, ...
                .map(Some)
                .collect_vec()
        );

        handle.await.unwrap();

        Ok(())
    }
}
