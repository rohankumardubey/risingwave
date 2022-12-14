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

use risingwave_common::catalog::{ColumnDesc, ColumnId, Field, Schema, TableId};
use risingwave_pb::plan_common::ColumnCatalog as ProstColumnCatalog;
use risingwave_pb::stream_plan::SinkNode;

use super::*;
use crate::executor::SinkExecutor;

pub struct SinkExecutorBuilder;

#[async_trait::async_trait]
impl ExecutorBuilder for SinkExecutorBuilder {
    type Node = SinkNode;

    async fn new_boxed_executor(
        params: ExecutorParams,
        node: &Self::Node,
        store: impl StateStore,
        stream: &mut LocalStreamManagerCore,
    ) -> StreamResult<BoxedExecutor> {
        let [materialize_executor]: [_; 1] = params.input.try_into().unwrap();

        let sink_desc = node.get_sink_desc().unwrap();

        let sink_config = sink_desc.get_sink_config();

        Ok(Box::new(SinkExecutor::new(
            materialize_executor,
            stream.streaming_metrics.clone(),
            sink_config.clone(),
            params.executor_id,
            params.env.connector_params(),
            schema,
            pk_indices,
        )))
    }
}
