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

use futures::StreamExt;
use futures_async_stream::try_stream;
use itertools::Itertools;
use risingwave_common::array::Op::*;
use risingwave_common::array::Row;
use risingwave_common::catalog::{ColumnId, Schema};
use risingwave_common::util::sort_util::OrderPair;
use risingwave_storage::{Keyspace, StateStore};

use crate::executor_v2::error::{StreamExecutorError, TracedStreamExecutorError};
use crate::executor_v2::mview::ManagedMViewState;
use crate::executor_v2::{
    BoxedExecutor, BoxedMessageStream, Executor, ExecutorInfo, Message, PkIndicesRef,
};

/// `MaterializeExecutor` materializes changes in stream into a materialized view on storage.
pub struct MaterializeExecutor<S: StateStore> {
    input: BoxedExecutor,

    local_state: ManagedMViewState<S>,

    /// Columns of arrange keys (including pk, group keys, join keys, etc.)
    arrange_columns: Vec<usize>,

    info: ExecutorInfo,
}

impl<S: StateStore> MaterializeExecutor<S> {
    pub fn new(
        input: BoxedExecutor,
        keyspace: Keyspace<S>,
        keys: Vec<OrderPair>,
        column_ids: Vec<ColumnId>,
        executor_id: u64,
    ) -> Self {
        let arrange_columns: Vec<usize> = keys.iter().map(|k| k.column_idx).collect();
        let arrange_order_types = keys.iter().map(|k| k.order_type).collect();
        let schema = input.schema().clone();
        Self {
            input,
            local_state: ManagedMViewState::new(keyspace, column_ids, arrange_order_types),
            arrange_columns: arrange_columns.clone(),
            info: ExecutorInfo {
                schema,
                pk_indices: arrange_columns,
                identity: format!("MaterializeExecutor {:X}", executor_id),
            },
        }
    }

    #[try_stream(ok = Message, error = TracedStreamExecutorError)]
    async fn execute_inner(mut self) {
        let input = self.input.execute();
        #[for_await]
        for msg in input {
            let msg = msg?;
            yield match msg {
                Message::Chunk(chunk) => {
                    for (idx, op) in chunk.ops().iter().enumerate() {
                        // check visibility
                        let visible = chunk
                            .visibility()
                            .as_ref()
                            .map(|x| x.is_set(idx).unwrap())
                            .unwrap_or(true);
                        if !visible {
                            continue;
                        }

                        // assemble pk row
                        let arrange_row = Row(self
                            .arrange_columns
                            .iter()
                            .map(|col_idx| chunk.column_at(*col_idx).array_ref().datum_at(idx))
                            .collect_vec());

                        // assemble row
                        let row = Row(chunk
                            .columns()
                            .iter()
                            .map(|x| x.array_ref().datum_at(idx))
                            .collect_vec());

                        match op {
                            Insert | UpdateInsert => {
                                self.local_state.put(arrange_row, row);
                            }
                            Delete | UpdateDelete => {
                                self.local_state.delete(arrange_row);
                            }
                        }
                    }

                    Message::Chunk(chunk)
                }
                Message::Barrier(b) => {
                    // FIXME(ZBW): use a better error type
                    self.local_state
                        .flush(b.epoch.prev)
                        .await
                        .map_err(StreamExecutorError::ExecutorV1)?;
                    Message::Barrier(b)
                }
            }
        }
    }
}

impl<S: StateStore> Executor for MaterializeExecutor<S> {
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
        self.info.identity.as_str()
    }
}

impl<S: StateStore> std::fmt::Debug for MaterializeExecutor<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MaterializeExecutor")
            .field("input info", &self.info())
            .field("arrange_columns", &self.arrange_columns)
            .finish()
    }
}

#[cfg(test)]
mod tests {

    use futures::stream::StreamExt;
    use risingwave_common::array::{I32Array, Op, Row};
    use risingwave_common::catalog::{ColumnDesc, Field, Schema, TableId};
    use risingwave_common::column_nonnull;
    use risingwave_common::types::DataType;
    use risingwave_common::util::sort_util::{OrderPair, OrderType};
    use risingwave_storage::memory::MemoryStateStore;
    use risingwave_storage::table::cell_based_table::CellBasedTable;
    use risingwave_storage::Keyspace;

    use crate::executor_v2::test_utils::*;
    use crate::executor_v2::*;

    #[tokio::test]
    async fn test_materialize_executor() {
        // Prepare storage and memtable.
        let memory_state_store = MemoryStateStore::new();
        let table_id = TableId::new(1);
        // Two columns of int32 type, the first column is PK.
        let schema = Schema::new(vec![
            Field::unnamed(DataType::Int32),
            Field::unnamed(DataType::Int32),
        ]);
        let column_ids = vec![0.into(), 1.into()];

        // Prepare source chunks.
        let chunk1 = StreamChunk::new(
            vec![Op::Insert, Op::Insert, Op::Insert],
            vec![
                column_nonnull! { I32Array, [1, 2, 3] },
                column_nonnull! { I32Array, [4, 5, 6] },
            ],
            None,
        );
        let chunk2 = StreamChunk::new(
            vec![Op::Insert, Op::Delete],
            vec![
                column_nonnull! { I32Array, [7, 3] },
                column_nonnull! { I32Array, [8, 6] },
            ],
            None,
        );

        // Prepare stream executors.
        let source = MockSource::with_messages(
            schema.clone(),
            PkIndices::new(),
            vec![
                Message::Chunk(chunk1),
                Message::Barrier(Barrier::default()),
                Message::Chunk(chunk2),
                Message::Barrier(Barrier::default()),
            ],
        );

        let keyspace = Keyspace::table_root(memory_state_store.clone(), &table_id);
        let order_types = vec![OrderType::Ascending];
        let column_descs = vec![ColumnDesc::unnamed(column_ids[1], DataType::Int32)];
        let table = CellBasedTable::new_for_test(keyspace.clone(), column_descs, order_types);
        let mut materialize_executor = Box::new(MaterializeExecutor::new(
            Box::new(source),
            keyspace,
            vec![OrderPair::new(0, OrderType::Ascending)],
            column_ids,
            1,
        ))
        .execute();

        materialize_executor.next().await.transpose().unwrap();

        // First stream chunk. We check the existence of (3) -> (3,6)
        match materialize_executor.next().await.transpose().unwrap() {
            Some(Message::Barrier(_)) => {
                let row = table
                    .get_row(&Row(vec![Some(3_i32.into())]), u64::MAX)
                    .await
                    .unwrap();
                assert_eq!(row, Some(Row(vec![Some(6_i32.into())])));
            }
            _ => unreachable!(),
        }
        materialize_executor.next().await.transpose().unwrap();
        // Second stream chunk. We check the existence of (7) -> (7,8)
        match materialize_executor.next().await.transpose().unwrap() {
            Some(Message::Barrier(_)) => {
                let row = table
                    .get_row(&Row(vec![Some(7_i32.into())]), u64::MAX)
                    .await
                    .unwrap();
                assert_eq!(row, Some(Row(vec![Some(8_i32.into())])));
            }
            _ => unreachable!(),
        }
    }
}
