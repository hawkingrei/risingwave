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

use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;
use futures::channel::mpsc::Sender;
use futures::SinkExt;
use itertools::Itertools;
use risingwave_common::array::Op;
use risingwave_common::hash::VIRTUAL_NODE_COUNT;
use risingwave_common::util::addr::{is_local_address, HostAddr};
use risingwave_common::util::hash_util::CRC32FastBuilder;
use tracing::event;

use super::{Barrier, Executor, Message, Mutation, Result, StreamChunk, StreamConsumer};
use crate::task::{ActorId, SharedContext};

/// `Output` provides an interface for `Dispatcher` to send data into downstream actors.
#[async_trait]
pub trait Output: Debug + Send + Sync + 'static {
    async fn send(&mut self, message: Message) -> Result<()>;

    fn actor_id(&self) -> ActorId;
}

type BoxedOutput = Box<dyn Output>;

/// `LocalOutput` sends data to a local `mpsc::Channel`
pub struct LocalOutput {
    actor_id: ActorId,

    ch: Sender<Message>,
}

impl Debug for LocalOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalOutput")
            .field("actor_id", &self.actor_id)
            .finish()
    }
}

impl LocalOutput {
    pub fn new(actor_id: ActorId, ch: Sender<Message>) -> Self {
        Self { actor_id, ch }
    }
}

#[async_trait]
impl Output for LocalOutput {
    async fn send(&mut self, message: Message) -> Result<()> {
        // local channel should never fail
        self.ch.send(message).await.unwrap();
        Ok(())
    }

    fn actor_id(&self) -> ActorId {
        self.actor_id
    }
}

/// `RemoteOutput` forwards data to`ExchangeServiceImpl`
pub struct RemoteOutput {
    actor_id: ActorId,

    ch: Sender<Message>,
}

impl Debug for RemoteOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteOutput")
            .field("actor_id", &self.actor_id)
            .finish()
    }
}

impl RemoteOutput {
    pub fn new(actor_id: ActorId, ch: Sender<Message>) -> Self {
        Self { actor_id, ch }
    }
}

#[async_trait]
impl Output for RemoteOutput {
    async fn send(&mut self, message: Message) -> Result<()> {
        let message = match message {
            Message::Chunk(chk) => Message::Chunk(chk.compact()?),
            _ => message,
        };
        // local channel should never fail
        self.ch.send(message).await.unwrap();
        Ok(())
    }

    fn actor_id(&self) -> ActorId {
        self.actor_id
    }
}

/// `DispatchExecutor` consumes messages and send them into downstream actors. Usually,
/// data chunks will be dispatched with some specified policy, while control message
/// such as barriers will be distributed to all receivers.
pub struct DispatchExecutor {
    input: Box<dyn Executor>,
    inner: DispatcherImpl,
    actor_id: u32,
    context: Arc<SharedContext>,
}

pub fn new_output(
    context: &SharedContext,
    addr: HostAddr,
    actor_id: u32,
    down_id: &u32,
) -> Result<Box<dyn Output>> {
    let tx = context.take_sender(&(actor_id, *down_id))?;
    if is_local_address(&addr, &context.addr) {
        // if this is a local downstream actor
        Ok(Box::new(LocalOutput::new(*down_id, tx)) as Box<dyn Output>)
    } else {
        Ok(Box::new(RemoteOutput::new(*down_id, tx)) as Box<dyn Output>)
    }
}

impl std::fmt::Debug for DispatchExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DispatchExecutor")
            .field("input", &self.input)
            .field("inner", &self.inner)
            .field("actor_id", &self.actor_id)
            .finish()
    }
}

impl DispatchExecutor {
    pub fn new(
        input: Box<dyn Executor>,
        inner: DispatcherImpl,
        actor_id: u32,
        context: Arc<SharedContext>,
    ) -> Self {
        Self {
            input,
            inner,
            actor_id,
            context,
        }
    }

    async fn dispatch(&mut self, msg: Message) -> Result<()> {
        match msg {
            Message::Chunk(chunk) => {
                self.inner.dispatch_data(chunk).await?;
            }
            Message::Barrier(barrier) => {
                let mutation = barrier.mutation.clone();
                self.pre_mutate_outputs(&mutation).await?;
                self.inner.dispatch_barrier(barrier).await?;
                self.post_mutate_outputs(&mutation).await?;
            }
        };
        Ok(())
    }

    /// For `Add` and `Update`, update the outputs before we dispatch the barrier.
    async fn pre_mutate_outputs(&mut self, mutation: &Option<Arc<Mutation>>) -> Result<()> {
        match mutation.as_deref() {
            Some(Mutation::UpdateOutputs(updates)) => {
                if let Some((_, actor_infos)) = updates.get_key_value(&self.actor_id) {
                    let mut new_outputs = vec![];

                    let actor_id = self.actor_id;
                    // delete the old local connections in both local and remote pools;
                    self.context.retain(|&(up_id, down_id)| {
                        up_id != actor_id || actor_infos.iter().any(|info| info.actor_id == down_id)
                    });

                    for actor_info in actor_infos.iter() {
                        let down_id = actor_info.get_actor_id();
                        let downstream_addr = actor_info.get_host()?.into();
                        new_outputs.push(new_output(
                            &self.context,
                            downstream_addr,
                            actor_id,
                            &down_id,
                        )?);
                    }
                    self.inner.set_outputs(new_outputs)
                }
            }
            Some(Mutation::AddOutput(adds)) => {
                if let Some(downstream_actor_infos) = adds.get(&self.actor_id) {
                    let mut outputs_to_add = Vec::with_capacity(downstream_actor_infos.len());
                    for downstream_actor_info in downstream_actor_infos {
                        let down_id = downstream_actor_info.get_actor_id();
                        let downstream_addr = downstream_actor_info.get_host()?.into();
                        outputs_to_add.push(new_output(
                            &self.context,
                            downstream_addr,
                            self.actor_id,
                            &down_id,
                        )?);
                    }
                    self.inner.add_outputs(outputs_to_add);
                }
            }
            _ => {}
        };

        Ok(())
    }

    /// For `Stop`, update the outputs after we dispatch the barrier.
    async fn post_mutate_outputs(&mut self, mutation: &Option<Arc<Mutation>>) -> Result<()> {
        #[allow(clippy::single_match)]
        match mutation.as_deref() {
            Some(Mutation::Stop(stops)) => {
                // Remove outputs only if this actor itself is not to be stopped.
                if !stops.contains(&self.actor_id) {
                    self.inner.remove_outputs(stops);
                }
            }
            _ => {}
        }

        Ok(())
    }
}

#[async_trait]
impl StreamConsumer for DispatchExecutor {
    async fn next(&mut self) -> Result<Option<Barrier>> {
        let msg = self.input.next().await?;
        let barrier = if let Message::Barrier(ref barrier) = msg {
            Some(barrier.clone())
        } else {
            None
        };
        self.dispatch(msg).await?;

        Ok(barrier)
    }
}

#[derive(Debug)]
pub enum DispatcherImpl {
    Hash(HashDataDispatcher),
    Broadcast(BroadcastDispatcher),
    Simple(SimpleDispatcher),
    RoundRobin(RoundRobinDataDispatcher),
}

macro_rules! impl_dispatcher {
    ([], $( { $variant_name:ident } ),*) => {
        impl DispatcherImpl {
            pub async fn dispatch_data(&mut self, chunk: StreamChunk) -> Result<()> {
                match self {
                    $( Self::$variant_name(inner) => inner.dispatch_data(chunk).await, )*
                }
            }

            pub async fn dispatch_barrier(&mut self, barrier: Barrier) -> Result<()> {
                match self {
                    $( Self::$variant_name(inner) => inner.dispatch_barrier(barrier).await, )*
                }
            }

            pub fn set_outputs(&mut self, outputs: impl IntoIterator<Item = BoxedOutput>) {
                match self {
                    $( Self::$variant_name(inner) => inner.set_outputs(outputs), )*
                }
            }

            pub fn add_outputs(&mut self, outputs: impl IntoIterator<Item = BoxedOutput>) {
                match self {
                    $(Self::$variant_name(inner) => inner.add_outputs(outputs), )*
                }
            }

            pub fn remove_outputs(&mut self, actor_ids: &HashSet<ActorId>) {
                match self {
                    $(Self::$variant_name(inner) => inner.remove_outputs(actor_ids), )*
                }
            }
        }
    }
}

macro_rules! for_all_dispatcher_variants {
    ($macro:ident $(, $x:tt)*) => {
        $macro! {
            [$($x), *],
            { Hash },
            { Broadcast },
            { Simple },
            { RoundRobin }
        }
    };
}

for_all_dispatcher_variants! { impl_dispatcher }

macro_rules! define_dispatcher_associated_types {
    () => {
        type DataFuture<'a> = impl DispatchFuture<'a>;
        type BarrierFuture<'a> = impl DispatchFuture<'a>;
    };
}

pub trait DispatchFuture<'a> = Future<Output = Result<()>> + Send;

pub trait Dispatcher: Debug + 'static {
    type DataFuture<'a>: DispatchFuture<'a>;
    type BarrierFuture<'a>: DispatchFuture<'a>;
    fn dispatch_data(&mut self, chunk: StreamChunk) -> Self::DataFuture<'_>;
    fn dispatch_barrier(&mut self, barrier: Barrier) -> Self::BarrierFuture<'_>;

    fn set_outputs(&mut self, outputs: impl IntoIterator<Item = BoxedOutput>);
    fn add_outputs(&mut self, outputs: impl IntoIterator<Item = BoxedOutput>);
    fn remove_outputs(&mut self, actor_ids: &HashSet<ActorId>);
}

pub struct RoundRobinDataDispatcher {
    outputs: Vec<BoxedOutput>,
    cur: usize,
}

impl Debug for RoundRobinDataDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoundRobinDataDispatcher")
            .field("outputs", &self.outputs)
            .finish()
    }
}

impl RoundRobinDataDispatcher {
    pub fn new(outputs: Vec<BoxedOutput>) -> Self {
        Self { outputs, cur: 0 }
    }
}

impl Dispatcher for RoundRobinDataDispatcher {
    define_dispatcher_associated_types!();

    fn dispatch_data(&mut self, chunk: StreamChunk) -> Self::DataFuture<'_> {
        async move {
            self.outputs[self.cur].send(Message::Chunk(chunk)).await?;
            self.cur += 1;
            self.cur %= self.outputs.len();
            Ok(())
        }
    }

    fn dispatch_barrier(&mut self, barrier: Barrier) -> Self::BarrierFuture<'_> {
        async move {
            // always broadcast barrier
            for output in &mut self.outputs {
                output.send(Message::Barrier(barrier.clone())).await?;
            }
            Ok(())
        }
    }

    fn set_outputs(&mut self, outputs: impl IntoIterator<Item = BoxedOutput>) {
        self.outputs = outputs.into_iter().collect();
        self.cur = self.cur.min(self.outputs.len() - 1);
    }

    fn add_outputs(&mut self, outputs: impl IntoIterator<Item = BoxedOutput>) {
        self.outputs.extend(outputs.into_iter());
    }

    fn remove_outputs(&mut self, actor_ids: &HashSet<ActorId>) {
        self.outputs
            .drain_filter(|output| actor_ids.contains(&output.actor_id()))
            .count();
    }
}

pub struct HashDataDispatcher {
    fragment_ids: Vec<u32>,
    outputs: Vec<BoxedOutput>,
    keys: Vec<usize>,
    /// Mapping from virtual node to actor id, used for hash data dispatcher to dispatch tasks to
    /// different downstream actors.
    hash_mapping: Vec<ActorId>,
}

impl Debug for HashDataDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HashDataDispatcher")
            .field("outputs", &self.outputs)
            .field("keys", &self.keys)
            .finish()
    }
}

impl HashDataDispatcher {
    pub fn new(
        fragment_ids: Vec<u32>,
        outputs: Vec<BoxedOutput>,
        keys: Vec<usize>,
        hash_mapping: Vec<ActorId>,
    ) -> Self {
        Self {
            fragment_ids,
            outputs,
            keys,
            hash_mapping,
        }
    }
}

impl Dispatcher for HashDataDispatcher {
    define_dispatcher_associated_types!();

    fn set_outputs(&mut self, outputs: impl IntoIterator<Item = BoxedOutput>) {
        self.outputs = outputs.into_iter().collect()
    }

    fn add_outputs(&mut self, outputs: impl IntoIterator<Item = BoxedOutput>) {
        self.outputs.extend(outputs.into_iter());
    }

    fn dispatch_barrier(&mut self, barrier: Barrier) -> Self::BarrierFuture<'_> {
        async move {
            // always broadcast barrier
            for output in &mut self.outputs {
                output.send(Message::Barrier(barrier.clone())).await?;
            }
            Ok(())
        }
    }

    fn dispatch_data(&mut self, chunk: StreamChunk) -> Self::DataFuture<'_> {
        async move {
            // A chunk can be shuffled into multiple output chunks that to be sent to downstreams.
            // In these output chunks, the only difference are visibility map, which is calculated
            // by the hash value of each line in the input chunk.
            let num_outputs = self.outputs.len();

            // get hash value of every line by its key
            let hash_builder = CRC32FastBuilder {};
            let hash_values = chunk
                .get_hash_values(&self.keys, hash_builder)
                .unwrap()
                .iter()
                .map(|hash| *hash as usize % VIRTUAL_NODE_COUNT)
                .collect::<Vec<_>>();

            let (ops, columns, visibility) = chunk.into_inner();

            let mut vis_maps = vec![vec![]; num_outputs];
            let mut last_hash_value_when_update_delete: usize = 0;
            let mut new_ops: Vec<Op> = Vec::with_capacity(ops.len());
            match visibility {
                None => {
                    hash_values.iter().zip_eq(ops).for_each(|(hash, op)| {
                        // get visibility map for every output chunk
                        for (output_idx, vis_map) in vis_maps.iter_mut().enumerate() {
                            vis_map.push(
                                self.hash_mapping[*hash] == self.outputs[output_idx].actor_id(),
                            );
                        }
                        // The 'update' message, noted by an UpdateDelete and a successive
                        // UpdateInsert, need to be rewritten to common
                        // Delete and Insert if they were dispatched to
                        // different actors.
                        if op == Op::UpdateDelete {
                            last_hash_value_when_update_delete = *hash;
                        } else if op == Op::UpdateInsert {
                            if *hash != last_hash_value_when_update_delete {
                                new_ops.push(Op::Delete);
                                new_ops.push(Op::Insert);
                            } else {
                                new_ops.push(Op::UpdateDelete);
                                new_ops.push(Op::UpdateInsert);
                            }
                        } else {
                            new_ops.push(op);
                        }
                    });
                }
                Some(visibility) => {
                    hash_values
                        .iter()
                        .zip_eq(visibility.iter())
                        .zip_eq(ops)
                        .for_each(|((hash, visible), op)| {
                            for (output_idx, vis_map) in vis_maps.iter_mut().enumerate() {
                                vis_map.push(
                                    visible
                                        && self.hash_mapping[*hash]
                                            == self.outputs[output_idx].actor_id(),
                                );
                            }
                            if !visible {
                                new_ops.push(op);
                                return;
                            }
                            if op == Op::UpdateDelete {
                                last_hash_value_when_update_delete = *hash;
                            } else if op == Op::UpdateInsert {
                                if *hash != last_hash_value_when_update_delete {
                                    new_ops.push(Op::Delete);
                                    new_ops.push(Op::Insert);
                                } else {
                                    new_ops.push(Op::UpdateDelete);
                                    new_ops.push(Op::UpdateInsert);
                                }
                            } else {
                                new_ops.push(op);
                            }
                        });
                }
            }

            let ops = new_ops;

            // individually output StreamChunk integrated with vis_map
            for ((vis_map, output), downstream) in vis_maps
                .into_iter()
                .zip_eq(self.outputs.iter_mut())
                .zip_eq(self.fragment_ids.iter())
            {
                let vis_map = vis_map.try_into().unwrap();
                // columns is not changed in this function
                let new_stream_chunk =
                    StreamChunk::new(ops.clone(), columns.clone(), Some(vis_map));
                if new_stream_chunk.cardinality() > 0 {
                    event!(
                        tracing::Level::TRACE,
                        msg = "chunk",
                        downstream = downstream,
                        "send = \n{:#?}",
                        new_stream_chunk
                    );
                    output.send(Message::Chunk(new_stream_chunk)).await?;
                }
            }
            Ok(())
        }
    }

    fn remove_outputs(&mut self, actor_ids: &HashSet<ActorId>) {
        self.outputs
            .drain_filter(|output| actor_ids.contains(&output.actor_id()))
            .count();
    }
}

/// `BroadcastDispatcher` dispatches message to all outputs.
pub struct BroadcastDispatcher {
    outputs: HashMap<ActorId, BoxedOutput>,
}

impl Debug for BroadcastDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BroadcastDispatcher")
            .field("outputs", &self.outputs)
            .finish()
    }
}

impl BroadcastDispatcher {
    pub fn new(outputs: impl IntoIterator<Item = BoxedOutput>) -> Self {
        Self {
            outputs: Self::into_pairs(outputs).collect(),
        }
    }

    fn into_pairs(
        outputs: impl IntoIterator<Item = BoxedOutput>,
    ) -> impl Iterator<Item = (ActorId, BoxedOutput)> {
        outputs
            .into_iter()
            .map(|output| (output.actor_id(), output))
    }
}

impl Dispatcher for BroadcastDispatcher {
    define_dispatcher_associated_types!();

    fn dispatch_data(&mut self, chunk: StreamChunk) -> Self::DataFuture<'_> {
        async move {
            for output in self.outputs.values_mut() {
                output.send(Message::Chunk(chunk.clone())).await?;
            }
            Ok(())
        }
    }

    fn dispatch_barrier(&mut self, barrier: Barrier) -> Self::BarrierFuture<'_> {
        async move {
            for output in self.outputs.values_mut() {
                output.send(Message::Barrier(barrier.clone())).await?;
            }
            Ok(())
        }
    }

    fn set_outputs(&mut self, outputs: impl IntoIterator<Item = BoxedOutput>) {
        self.outputs = Self::into_pairs(outputs).collect()
    }

    fn add_outputs(&mut self, outputs: impl IntoIterator<Item = BoxedOutput>) {
        self.outputs.extend(Self::into_pairs(outputs));
    }

    fn remove_outputs(&mut self, actor_ids: &HashSet<ActorId>) {
        self.outputs
            .drain_filter(|actor_id, _| actor_ids.contains(actor_id))
            .count();
    }
}

/// `SimpleDispatcher` dispatches message to a single output.
pub struct SimpleDispatcher {
    output: BoxedOutput,
}

impl Debug for SimpleDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimpleDispatcher")
            .field("output", &self.output)
            .finish()
    }
}

impl SimpleDispatcher {
    pub fn new(output: BoxedOutput) -> Self {
        Self { output }
    }
}

impl Dispatcher for SimpleDispatcher {
    define_dispatcher_associated_types!();

    fn set_outputs(&mut self, outputs: impl IntoIterator<Item = BoxedOutput>) {
        self.output = outputs.into_iter().next().unwrap();
    }

    fn add_outputs(&mut self, outputs: impl IntoIterator<Item = BoxedOutput>) {
        self.output = outputs.into_iter().next().unwrap();
    }

    fn dispatch_barrier(&mut self, barrier: Barrier) -> Self::BarrierFuture<'_> {
        async move {
            self.output.send(Message::Barrier(barrier.clone())).await?;
            Ok(())
        }
    }

    fn dispatch_data(&mut self, chunk: StreamChunk) -> Self::DataFuture<'_> {
        async move {
            self.output.send(Message::Chunk(chunk)).await?;
            Ok(())
        }
    }

    fn remove_outputs(&mut self, actor_ids: &HashSet<ActorId>) {
        if actor_ids.contains(&self.output.actor_id()) {
            panic!("cannot remove outputs from SimpleDispatcher");
        }
    }
}

#[cfg(test)]
mod sender_consumer {
    use super::*;
    /// `SenderConsumer` consumes data from input executor and send it into a channel.
    #[derive(Debug)]
    pub struct SenderConsumer {
        input: Box<dyn Executor>,
        channel: BoxedOutput,
    }

    impl SenderConsumer {
        pub fn new(input: Box<dyn Executor>, channel: BoxedOutput) -> Self {
            Self { input, channel }
        }
    }

    #[async_trait]
    impl StreamConsumer for SenderConsumer {
        async fn next(&mut self) -> Result<Option<Barrier>> {
            let message = self.input.next().await?;
            let barrier = if let Message::Barrier(ref barrier) = message {
                Some(barrier.clone())
            } else {
                None
            };
            self.channel.send(message).await?;
            Ok(barrier)
        }
    }
}

#[cfg(test)]
pub use sender_consumer::SenderConsumer;

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::hash::{BuildHasher, Hasher};
    use std::sync::{Arc, Mutex};

    use futures::channel::mpsc::channel;
    use itertools::Itertools;
    use risingwave_common::array::column::Column;
    use risingwave_common::array::{Array, ArrayBuilder, I32ArrayBuilder, I64Array, Op};
    use risingwave_common::buffer::Bitmap;
    use risingwave_common::catalog::Schema;
    use risingwave_common::column_nonnull;
    use risingwave_common::hash::VIRTUAL_NODE_COUNT;
    use risingwave_pb::common::{ActorInfo, HostAddress};

    use super::*;
    use crate::executor_v2::receiver::ReceiverExecutor;
    use crate::executor_v2::Executor;
    use crate::task::{LOCAL_OUTPUT_CHANNEL_SIZE, LOCAL_TEST_ADDR};

    #[derive(Debug)]
    pub struct MockOutput {
        actor_id: ActorId,
        data: Arc<Mutex<Vec<Message>>>,
    }

    impl MockOutput {
        pub fn new(actor_id: ActorId, data: Arc<Mutex<Vec<Message>>>) -> Self {
            Self { actor_id, data }
        }
    }

    #[async_trait]
    impl Output for MockOutput {
        async fn send(&mut self, message: Message) -> Result<()> {
            self.data.lock().unwrap().push(message);
            Ok(())
        }

        fn actor_id(&self) -> ActorId {
            self.actor_id
        }
    }

    #[tokio::test]
    async fn test_hash_dispatcher_complex() {
        test_hash_dispatcher_complex_inner().await
    }

    async fn test_hash_dispatcher_complex_inner() {
        let num_outputs = 2; // actor id ranges from 1 to 2
        let key_indices = &[0, 2];
        let output_data_vecs = (0..num_outputs)
            .map(|_| Arc::new(Mutex::new(Vec::new())))
            .collect::<Vec<_>>();
        let outputs = output_data_vecs
            .iter()
            .enumerate()
            .map(|(actor_id, data)| {
                Box::new(MockOutput::new(1 + actor_id as u32, data.clone())) as BoxedOutput
            })
            .collect::<Vec<_>>();
        let mut hash_mapping = (1..num_outputs + 1)
            .flat_map(|id| vec![id as ActorId; VIRTUAL_NODE_COUNT / num_outputs])
            .collect_vec();
        hash_mapping.resize(VIRTUAL_NODE_COUNT, num_outputs as u32);
        let mut hash_dispatcher = HashDataDispatcher::new(
            (0..outputs.len() as u32).collect(),
            outputs,
            key_indices.to_vec(),
            hash_mapping,
        );

        let chunk = StreamChunk::new(
            vec![
                Op::Insert,
                Op::Insert,
                Op::Insert,
                Op::Delete,
                Op::UpdateDelete,
                Op::UpdateInsert,
                Op::UpdateDelete,
                Op::UpdateInsert,
            ],
            vec![
                column_nonnull! { I64Array, [4, 5, 0, 1, 2, 2, 3, 3] },
                column_nonnull! { I64Array, [6, 7, 0, 1, 0, 0, 3, 3] },
                column_nonnull! { I64Array, [8, 9, 0, 1, 2, 2, 2, 4] },
            ],
            Some(Bitmap::try_from(vec![true, true, true, false, true, true, true, true]).unwrap()),
        );

        hash_dispatcher.dispatch_data(chunk).await.unwrap();

        {
            let guard = output_data_vecs[0].lock().unwrap();
            match guard[0] {
                Message::Chunk(ref chunk1) => {
                    assert_eq!(chunk1.capacity(), 8, "Should keep capacity");
                    assert_eq!(chunk1.cardinality(), 5);
                    assert!(chunk1.visibility().as_ref().unwrap().is_set(4).unwrap());
                    assert_eq!(
                        chunk1.ops()[6],
                        Op::Delete,
                        "Should rewrite UpdateDelete to Delete"
                    );
                }
                _ => unreachable!(),
            }
        }
        {
            let guard = output_data_vecs[1].lock().unwrap();
            match guard[0] {
                Message::Chunk(ref chunk1) => {
                    assert_eq!(chunk1.capacity(), 8, "Should keep capacity");
                    assert_eq!(chunk1.cardinality(), 2);
                    assert!(
                        !chunk1.visibility().as_ref().unwrap().is_set(3).unwrap(),
                        "Should keep original invisible mark"
                    );
                    assert!(!chunk1.visibility().as_ref().unwrap().is_set(6).unwrap());

                    assert_eq!(
                        chunk1.ops()[4],
                        Op::UpdateDelete,
                        "Should keep UpdateDelete"
                    );
                    assert_eq!(
                        chunk1.ops()[5],
                        Op::UpdateInsert,
                        "Should keep UpdateInsert"
                    );

                    assert_eq!(
                        chunk1.ops()[7],
                        Op::Insert,
                        "Should rewrite UpdateInsert to Insert"
                    );
                }
                _ => unreachable!(),
            }
        }
    }

    fn add_local_channels(ctx: Arc<SharedContext>, up_down_ids: Vec<(u32, u32)>) {
        for up_down_id in up_down_ids {
            let (tx, rx) = channel(LOCAL_OUTPUT_CHANNEL_SIZE);
            ctx.add_channel_pairs(up_down_id, (Some(tx), Some(rx)));
        }
    }

    fn add_remote_channels(ctx: Arc<SharedContext>, up_id: u32, down_ids: Vec<u32>) {
        for down_id in down_ids {
            let (tx, rx) = channel(LOCAL_OUTPUT_CHANNEL_SIZE);
            ctx.add_channel_pairs((up_id, down_id), (Some(tx), Some(rx)));
        }
    }

    fn helper_make_local_actor(actor_id: u32) -> ActorInfo {
        ActorInfo {
            actor_id,
            host: Some(HostAddress {
                host: LOCAL_TEST_ADDR.host.clone(),
                port: LOCAL_TEST_ADDR.port as i32,
            }),
        }
    }

    fn helper_make_remote_actor(actor_id: u32) -> ActorInfo {
        ActorInfo {
            actor_id,
            host: Some(HostAddress {
                host: "172.1.1.2".to_string(),
                port: 2334,
            }),
        }
    }

    #[tokio::test]
    async fn test_configuration_change() {
        let schema = Schema { fields: vec![] };
        let (mut tx, rx) = channel(16);
        let input = Box::new(ReceiverExecutor::new(schema.clone(), vec![], rx)).v1();
        let data_sink = Arc::new(Mutex::new(vec![]));
        let actor_id = 233;
        let output = Box::new(MockOutput::new(actor_id, data_sink));
        let ctx = Arc::new(SharedContext::for_test());

        let mut executor = Box::new(DispatchExecutor::new(
            Box::new(input),
            DispatcherImpl::Simple(SimpleDispatcher::new(output)),
            actor_id,
            ctx.clone(),
        ));
        let mut updates1: HashMap<u32, Vec<ActorInfo>> = HashMap::new();

        updates1.insert(
            actor_id,
            vec![
                helper_make_local_actor(234),
                helper_make_local_actor(235),
                helper_make_remote_actor(238),
            ],
        );
        add_local_channels(ctx.clone(), vec![(233, 234), (233, 235)]);
        add_remote_channels(ctx.clone(), 233, vec![238]);

        let b1 = Barrier::new_test_barrier(1).with_mutation(Mutation::UpdateOutputs(updates1));
        tx.send(Message::Barrier(b1)).await.unwrap();
        executor.next().await.unwrap();
        let tctx = ctx.clone();
        {
            assert_eq!(tctx.get_channel_pair_number(), 3);
        }

        let mut updates2: HashMap<u32, Vec<ActorInfo>> = HashMap::new();
        updates2.insert(actor_id, vec![helper_make_local_actor(235)]);
        add_local_channels(ctx.clone(), vec![(233, 235)]);
        let b2 = Barrier::new_test_barrier(1).with_mutation(Mutation::UpdateOutputs(updates2));

        tx.send(Message::Barrier(b2)).await.unwrap();
        executor.next().await.unwrap();
        let tctx = ctx.clone();
        {
            assert_eq!(tctx.get_channel_pair_number(), 1);
        }

        add_local_channels(ctx.clone(), vec![(233, 245)]);
        add_remote_channels(ctx.clone(), 233, vec![246]);
        tx.send(Message::Barrier(
            Barrier::new_test_barrier(1).with_mutation(Mutation::AddOutput({
                let mut actors = HashMap::default();
                actors.insert(
                    233,
                    vec![helper_make_local_actor(245), helper_make_remote_actor(246)],
                );
                actors
            })),
        ))
        .await
        .unwrap();
        executor.next().await.unwrap();
        let tctx = ctx.clone();
        {
            assert_eq!(tctx.get_channel_pair_number(), 3);
        }
    }

    #[tokio::test]
    async fn test_hash_dispatcher() {
        let num_outputs = 5; // actor id ranges from 1 to 5
        let cardinality = 10;
        let dimension = 4;
        let key_indices = &[0, 2];
        let output_data_vecs = (0..num_outputs)
            .map(|_| Arc::new(Mutex::new(Vec::new())))
            .collect::<Vec<_>>();
        let outputs = output_data_vecs
            .iter()
            .enumerate()
            .map(|(actor_id, data)| {
                Box::new(MockOutput::new(1 + actor_id as u32, data.clone())) as BoxedOutput
            })
            .collect::<Vec<_>>();
        let mut hash_mapping = (1..num_outputs + 1)
            .flat_map(|id| vec![id as ActorId; VIRTUAL_NODE_COUNT / num_outputs])
            .collect_vec();
        hash_mapping.resize(VIRTUAL_NODE_COUNT, num_outputs as u32);
        let mut hash_dispatcher = HashDataDispatcher::new(
            (0..outputs.len() as u32).collect(),
            outputs,
            key_indices.to_vec(),
            hash_mapping.clone(),
        );

        let mut ops = Vec::new();
        for idx in 0..cardinality {
            if idx % 2 == 0 {
                ops.push(Op::Insert);
            } else {
                ops.push(Op::Delete);
            }
        }

        let mut start = 19260817i32..;
        let mut builders = (0..dimension)
            .map(|_| I32ArrayBuilder::new(cardinality).unwrap())
            .collect_vec();
        let mut output_cols = vec![vec![vec![]; dimension]; num_outputs];
        let mut output_ops = vec![vec![]; num_outputs];
        for op in &ops {
            let hash_builder = CRC32FastBuilder {};
            let mut hasher = hash_builder.build_hasher();
            let one_row = (0..dimension).map(|_| start.next().unwrap()).collect_vec();
            for key_idx in key_indices.iter() {
                let val = one_row[*key_idx];
                let bytes = val.to_le_bytes();
                hasher.update(&bytes);
            }
            let output_idx =
                hash_mapping[hasher.finish() as usize % VIRTUAL_NODE_COUNT] as usize - 1;
            for (builder, val) in builders.iter_mut().zip_eq(one_row.iter()) {
                builder.append(Some(*val)).unwrap();
            }
            output_cols[output_idx]
                .iter_mut()
                .zip_eq(one_row.iter())
                .for_each(|(each_column, val)| each_column.push(*val));
            output_ops[output_idx].push(op);
        }

        let columns = builders
            .into_iter()
            .map(|builder| {
                let array = builder.finish().unwrap();
                Column::new(Arc::new(array.into()))
            })
            .collect::<Vec<_>>();

        let chunk = StreamChunk::new(ops, columns, None);
        hash_dispatcher.dispatch_data(chunk).await.unwrap();

        for (output_idx, output) in output_data_vecs.into_iter().enumerate() {
            let guard = output.lock().unwrap();
            // It is possible that there is no chunks, as a key doesn't belong to any hash bucket.
            assert!(guard.len() <= 1);
            if guard.is_empty() {
                assert!(output_cols[output_idx].iter().all(|x| { x.is_empty() }));
            } else {
                let message = guard.get(0).unwrap();
                let real_chunk = match message {
                    Message::Chunk(chunk) => chunk,
                    _ => panic!(),
                };
                real_chunk
                    .columns()
                    .iter()
                    .zip_eq(output_cols[output_idx].iter())
                    .for_each(|(real_col, expect_col)| {
                        let real_vals = real_chunk
                            .visibility()
                            .as_ref()
                            .unwrap()
                            .iter()
                            .enumerate()
                            .filter(|(_, vis)| *vis)
                            .map(|(row_idx, _)| {
                                real_col.array_ref().as_int32().value_at(row_idx).unwrap()
                            })
                            .collect::<Vec<_>>();
                        assert_eq!(real_vals.len(), expect_col.len());
                        assert_eq!(real_vals, *expect_col);
                    });
            }
        }
    }
}
