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

use std::sync::Arc;

use futures::StreamExt;
use futures_async_stream::try_stream;
use itertools::Itertools;
use risingwave_common::array::column::Column;
use risingwave_common::array::StreamChunk;
use risingwave_common::catalog::Schema;
use risingwave_common::error::Result;
use risingwave_storage::{Keyspace, StateStore};

use super::{Executor, ExecutorInfo, StreamExecutorResult};
use crate::executor::{pk_input_array_refs, PkIndicesRef};
use crate::executor_v2::aggregation::{
    agg_input_array_refs, generate_agg_schema, generate_agg_state, AggCall, AggState,
};
use crate::executor_v2::error::{StreamExecutorError, TracedStreamExecutorError};
use crate::executor_v2::{BoxedMessageStream, Message, PkIndices};

/// `SimpleAggExecutor` is the aggregation operator for streaming system.
/// To create an aggregation operator, states and expressions should be passed along the
/// constructor.
///
/// `SimpleAggExecutor` maintain multiple states together. If there are `n`
/// states and `n` expressions, there will be `n` columns as output.
///
/// As the engine processes data in chunks, it is possible that multiple update
/// messages could consolidate to a single row update. For example, our source
/// emits 1000 inserts in one chunk, and we aggregates count function on that.
/// Current `SimpleAggExecutor` will only emit one row for a whole chunk.
/// Therefore, we "automatically" implemented a window function inside
/// `SimpleAggExecutor`.
pub struct SimpleAggExecutor<S: StateStore> {
    input: Box<dyn Executor>,
    info: ExecutorInfo,

    /// Pk indices from input
    input_pk_indices: Vec<usize>,

    /// Schema from input
    input_schema: Schema,

    /// The executor operates on this keyspace.
    keyspace: Keyspace<S>,

    /// Aggregation states of the current operator.
    /// This is an `Option` and the initial state is built when `Executor::next` is called, since
    /// we may not want `Self::new` to be an `async` function.
    states: Option<AggState<S>>,

    /// An operator will support multiple aggregation calls.
    agg_calls: Vec<AggCall>,

    #[allow(dead_code)]
    /// Indices of the columns on which key distribution depends.
    key_indices: Vec<usize>,
}

impl<S: StateStore> Executor for SimpleAggExecutor<S> {
    fn execute(self: Box<Self>) -> BoxedMessageStream {
        self.execute_inner().boxed()
    }

    fn schema(&self) -> &Schema {
        &self.info.schema
    }

    fn pk_indices(&self) -> PkIndicesRef {
        &self.info.pk_indices
    }

    fn identity(&self) -> &str {
        &self.info.identity
    }
}

impl<S: StateStore> SimpleAggExecutor<S> {
    pub fn new(
        input: Box<dyn Executor>,
        agg_calls: Vec<AggCall>,
        keyspace: Keyspace<S>,
        pk_indices: PkIndices,
        executor_id: u64,
        key_indices: Vec<usize>,
    ) -> Result<Self> {
        let input_info = input.info();
        let schema = generate_agg_schema(input.as_ref(), &agg_calls, None);

        Ok(Self {
            input,
            info: ExecutorInfo {
                schema,
                pk_indices,
                identity: format!("SimpleAggExecutor-{:X}", executor_id),
            },
            input_pk_indices: input_info.pk_indices,
            input_schema: input_info.schema,
            keyspace,
            states: None,
            agg_calls,
            key_indices,
        })
    }

    async fn apply_chunk(
        agg_calls: &[AggCall],
        input_pk_indices: &[usize],
        input_schema: &Schema,
        states: &mut Option<AggState<S>>,
        keyspace: &Keyspace<S>,
        chunk: StreamChunk,
        epoch: u64,
    ) -> StreamExecutorResult<()> {
        let (ops, columns, visibility) = chunk.into_inner();

        // --- Retrieve all aggregation inputs in advance ---
        let all_agg_input_arrays = agg_input_array_refs(agg_calls, &columns);
        let pk_input_arrays = pk_input_array_refs(input_pk_indices, &columns);
        let input_pk_data_types = input_pk_indices
            .iter()
            .map(|idx| input_schema.fields[*idx].data_type.clone())
            .collect();

        // When applying batch, we will send columns of primary keys to the last N columns.
        let all_agg_data = all_agg_input_arrays
            .into_iter()
            .map(|mut input_arrays| {
                input_arrays.extend(pk_input_arrays.iter());
                input_arrays
            })
            .collect_vec();

        // 1. Retrieve previous state from the KeyedState. If they didn't exist, the ManagedState
        // will automatically create new ones for them.
        if states.is_none() {
            let state =
                generate_agg_state(None, agg_calls, keyspace, input_pk_data_types, epoch, None)
                    .await?;
            *states = Some(state);
        }
        let states = states.as_mut().unwrap();

        // 2. Mark the state as dirty by filling prev states
        states
            .may_mark_as_dirty(epoch)
            .await
            .map_err(StreamExecutorError::agg_state_error)?;

        // 3. Apply batch to each of the state (per agg_call)
        for (agg_state, data) in states.managed_states.iter_mut().zip_eq(all_agg_data.iter()) {
            agg_state
                .apply_batch(&ops, visibility.as_ref(), data, epoch)
                .await
                .map_err(StreamExecutorError::agg_state_error)?;
        }

        Ok(())
    }

    async fn flush_data(
        schema: &Schema,
        states: &mut Option<AggState<S>>,
        keyspace: &Keyspace<S>,
        epoch: u64,
    ) -> StreamExecutorResult<Option<StreamChunk>> {
        // --- Flush states to the state store ---
        // Some state will have the correct output only after their internal states have been fully
        // flushed.

        let states = match states.as_mut() {
            Some(states) if states.is_dirty() => states,
            _ => return Ok(None), // Nothing to flush.
        };

        let mut write_batch = keyspace.state_store().start_write_batch();
        for state in &mut states.managed_states {
            state
                .flush(&mut write_batch)
                .map_err(StreamExecutorError::agg_state_error)?;
        }
        write_batch
            .ingest(epoch)
            .await
            .map_err(StreamExecutorError::agg_state_error)?;

        // --- Create array builders ---
        // As the datatype is retrieved from schema, it contains both group key and aggregation
        // state outputs.
        let mut builders = schema
            .create_array_builders(2)
            .map_err(StreamExecutorError::eval_error)?;
        let mut new_ops = Vec::with_capacity(2);

        // --- Retrieve modified states and put the changes into the builders ---
        states
            .build_changes(&mut builders, &mut new_ops, epoch)
            .await
            .map_err(StreamExecutorError::agg_state_error)?;

        let columns: Vec<Column> = builders
            .into_iter()
            .map(|builder| -> Result<_> { Ok(Column::new(Arc::new(builder.finish()?))) })
            .try_collect()
            .map_err(StreamExecutorError::eval_error)?;

        let chunk = StreamChunk::new(new_ops, columns, None);

        Ok(Some(chunk))
    }

    #[try_stream(ok = Message, error = TracedStreamExecutorError)]
    async fn execute_inner(self) {
        let SimpleAggExecutor {
            input,
            info,
            input_pk_indices,
            input_schema,
            keyspace,
            mut states,
            agg_calls,
            key_indices: _,
        } = self;
        let mut input = input.execute();
        let first_msg = input.next().await.unwrap()?;
        let barrier = first_msg
            .into_barrier()
            .expect("the first message received by agg executor must be a barrier");
        let mut epoch = barrier.epoch.curr;
        yield Message::Barrier(barrier);

        #[for_await]
        for msg in input {
            let msg = msg?;
            match msg {
                Message::Chunk(chunk) => {
                    Self::apply_chunk(
                        &agg_calls,
                        &input_pk_indices,
                        &input_schema,
                        &mut states,
                        &keyspace,
                        chunk,
                        epoch,
                    )
                    .await?;
                }
                Message::Barrier(barrier) => {
                    let next_epoch = barrier.epoch.curr;
                    if let Some(chunk) =
                        Self::flush_data(&info.schema, &mut states, &keyspace, epoch).await?
                    {
                        assert_eq!(epoch, barrier.epoch.prev);
                        yield Message::Chunk(chunk);
                    }
                    yield Message::Barrier(barrier);
                    epoch = next_epoch;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use futures::StreamExt;
    use global_simple_agg::*;
    use risingwave_common::array::{I64Array, Op, Row};
    use risingwave_common::catalog::Field;
    use risingwave_common::column_nonnull;
    use risingwave_common::types::*;
    use risingwave_expr::expr::*;

    use crate::executor_v2::aggregation::AggArgs;
    use crate::executor_v2::test_utils::*;
    use crate::executor_v2::*;
    use crate::*;

    #[tokio::test]
    async fn test_local_simple_aggregation_in_memory() {
        test_local_simple_aggregation(create_in_memory_keyspace()).await
    }

    async fn test_local_simple_aggregation(keyspace: Keyspace<impl StateStore>) {
        let chunk1 = StreamChunk::new(
            vec![Op::Insert, Op::Insert, Op::Insert],
            vec![
                column_nonnull! { I64Array, [100, 10, 4] },
                column_nonnull! { I64Array, [200, 14, 300] },
                // primary key column
                column_nonnull! { I64Array, [1001, 1002, 1003] },
            ],
            None,
        );
        let chunk2 = StreamChunk::new(
            vec![Op::Delete, Op::Delete, Op::Delete, Op::Insert],
            vec![
                column_nonnull! { I64Array, [100, 10, 4, 104] },
                column_nonnull! { I64Array, [200, 14, 300, 500] },
                // primary key column
                column_nonnull! { I64Array, [1001, 1002, 1003, 1004] },
            ],
            Some((vec![true, false, true, true]).try_into().unwrap()),
        );
        let schema = Schema {
            fields: vec![
                Field::unnamed(DataType::Int64),
                Field::unnamed(DataType::Int64),
                // primary key column`
                Field::unnamed(DataType::Int64),
            ],
        };

        let mut source = MockSource::new(schema, vec![2]); // pk
        source.push_barrier(1, false);
        source.push_chunks([chunk1].into_iter());
        source.push_barrier(2, false);
        source.push_chunks([chunk2].into_iter());
        source.push_barrier(3, false);

        // This is local simple aggregation, so we add another row count state
        let agg_calls = vec![
            AggCall {
                kind: AggKind::RowCount,
                args: AggArgs::None,
                return_type: DataType::Int64,
            },
            AggCall {
                kind: AggKind::Sum,
                args: AggArgs::Unary(DataType::Int64, 0),
                return_type: DataType::Int64,
            },
            AggCall {
                kind: AggKind::Sum,
                args: AggArgs::Unary(DataType::Int64, 1),
                return_type: DataType::Int64,
            },
            AggCall {
                kind: AggKind::Min,
                args: AggArgs::Unary(DataType::Int64, 0),
                return_type: DataType::Int64,
            },
        ];

        let simple_agg = Box::new(
            SimpleAggExecutor::new(Box::new(source), agg_calls, keyspace, vec![], 1, vec![])
                .unwrap(),
        );
        let mut simple_agg = simple_agg.execute();

        // Consume the init barrier
        simple_agg.next().await.unwrap().unwrap();
        // Consume stream chunk
        let msg = simple_agg.next().await.unwrap().unwrap();
        if let Message::Chunk(chunk) = msg {
            let (data_chunk, ops) = chunk.into_parts();
            let rows = ops
                .into_iter()
                .zip_eq(data_chunk.rows().map(Row::from))
                .collect_vec();
            let expected_rows = [(Op::Insert, row_nonnull![3_i64, 114_i64, 514_i64, 4_i64])];

            assert_eq!(rows, expected_rows);
        } else {
            unreachable!("unexpected message {:?}", msg);
        }

        assert_matches!(
            simple_agg.next().await.unwrap().unwrap(),
            Message::Barrier { .. }
        );

        let msg = simple_agg.next().await.unwrap().unwrap();
        if let Message::Chunk(chunk) = msg {
            let (data_chunk, ops) = chunk.into_parts();
            let rows = ops
                .into_iter()
                .zip_eq(data_chunk.rows().map(Row::from))
                .collect_vec();
            let expected_rows = [
                (
                    Op::UpdateDelete,
                    row_nonnull![3_i64, 114_i64, 514_i64, 4_i64],
                ),
                (
                    Op::UpdateInsert,
                    row_nonnull![2_i64, 114_i64, 514_i64, 10_i64],
                ),
            ];

            assert_eq!(rows, expected_rows);
        } else {
            unreachable!("unexpected message {:?}", msg);
        }
    }
}
