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

//! Streaming Aggregators

use itertools::Itertools;
use risingwave_common::error::Result;
use risingwave_common::try_match_expand;
use risingwave_pb::stream_plan;
use risingwave_pb::stream_plan::stream_node::Node;
use risingwave_storage::{Keyspace, StateStore};

use crate::executor::{Executor, ExecutorBuilder};
use crate::executor_v2::aggregation::AggCall;
use crate::executor_v2::{Executor as ExecutorV2, SimpleAggExecutor as SimpleAggExecutorV2};
use crate::task::{build_agg_call_from_prost, ExecutorParams, LocalStreamManagerCore};

pub struct SimpleAggExecutorBuilder {}

impl ExecutorBuilder for SimpleAggExecutorBuilder {
    fn new_boxed_executor(
        mut params: ExecutorParams,
        node: &stream_plan::StreamNode,
        store: impl StateStore,
        _stream: &mut LocalStreamManagerCore,
    ) -> Result<Box<dyn Executor>> {
        let node = try_match_expand!(node.get_node().unwrap(), Node::GlobalSimpleAggNode)?;
        let agg_calls: Vec<AggCall> = node
            .get_agg_calls()
            .iter()
            .map(build_agg_call_from_prost)
            .try_collect()?;
        let keyspace = Keyspace::executor_root(store, params.executor_id);
        let key_indices = node
            .get_distribution_keys()
            .iter()
            .map(|key| *key as usize)
            .collect::<Vec<_>>();
        Ok(Box::new(
            Box::new(SimpleAggExecutorV2::new_from_v1(
                params.input.remove(0),
                agg_calls,
                keyspace,
                params.pk_indices,
                params.executor_id,
                params.op_info,
                key_indices,
            )?)
            .v1(),
        ))
    }
}
