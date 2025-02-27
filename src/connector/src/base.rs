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

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use bytes::Bytes;
use itertools::Itertools;
use kafka::enumerator::KafkaSplitEnumerator;
use serde::{Deserialize, Serialize};

use crate::kafka::source::KafkaSplitReader;
use crate::kinesis::source::reader::KinesisSplitReader;

pub enum SourceOffset {
    Number(i64),
    String(String),
}

use crate::kafka::KafkaSplit;
use crate::kinesis::split::KinesisSplit;
use crate::pulsar::{PulsarSplit, PulsarSplitEnumerator};
use crate::utils::AnyhowProperties;
use crate::{kafka, kinesis, pulsar, Properties};

const UPSTREAM_SOURCE_KEY: &str = "connector";
const KAFKA_SOURCE: &str = "kafka";
const KINESIS_SOURCE: &str = "kinesis";
const PULSAR_SOURCE: &str = "pulsar";

pub trait SourceMessage {
    fn payload(&self) -> Result<Option<&[u8]>>;
    fn offset(&self) -> Result<Option<SourceOffset>>;
    fn serialize(&self) -> Result<String>;
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct InnerMessage {
    pub payload: Option<Bytes>,
    pub offset: String,
    pub split_id: String,
}

pub trait SourceSplit: Sized {
    fn id(&self) -> String;
    fn to_string(&self) -> Result<String>;
    fn restore_from_bytes(bytes: &[u8]) -> Result<Self>;
    fn get_type(&self) -> String;
}

#[derive(Debug, Clone)]
pub struct ConnectorState {
    pub identifier: Bytes,
    pub start_offset: String,
    pub end_offset: String,
}

#[async_trait]
pub trait SourceReader {
    async fn next(&mut self) -> Result<Option<Vec<InnerMessage>>>;
    async fn new(properties: Properties, state: Option<ConnectorState>) -> Result<Self>
    where
        Self: Sized;
}

#[async_trait]
pub trait SplitEnumerator {
    type Split: SourceSplit + Send + Sync;
    async fn list_splits(&mut self) -> Result<Vec<Self::Split>>;
}

pub enum SplitEnumeratorImpl {
    Kafka(kafka::enumerator::KafkaSplitEnumerator),
    Pulsar(pulsar::enumerator::PulsarSplitEnumerator),
    Kinesis(kinesis::enumerator::client::KinesisSplitEnumerator),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SplitImpl {
    Kafka(kafka::KafkaSplit),
    Pulsar(pulsar::PulsarSplit),
    Kinesis(kinesis::split::KinesisSplit),
}

impl SplitImpl {
    pub fn id(&self) -> String {
        match self {
            SplitImpl::Kafka(k) => k.id(),
            SplitImpl::Pulsar(p) => p.id(),
            SplitImpl::Kinesis(k) => k.id(),
        }
    }

    pub fn to_string(&self) -> Result<String> {
        match self {
            SplitImpl::Kafka(k) => k.to_string(),
            SplitImpl::Pulsar(p) => p.to_string(),
            SplitImpl::Kinesis(k) => k.to_string(),
        }
    }

    pub fn get_type(&self) -> String {
        match self {
            SplitImpl::Kafka(k) => k.get_type(),
            SplitImpl::Pulsar(p) => p.get_type(),
            SplitImpl::Kinesis(k) => k.get_type(),
        }
    }

    pub fn restore_from_bytes(split_type: String, bytes: &[u8]) -> Result<Self> {
        match split_type.as_str() {
            kafka::KAFKA_SPLIT_TYPE => KafkaSplit::restore_from_bytes(bytes).map(SplitImpl::Kafka),
            pulsar::PULSAR_SPLIT_TYPE => {
                PulsarSplit::restore_from_bytes(bytes).map(SplitImpl::Pulsar)
            }
            kinesis::split::KINESIS_SPLIT_TYPE => {
                KinesisSplit::restore_from_bytes(bytes).map(SplitImpl::Kinesis)
            }
            other => Err(anyhow!("split type {} not supported", other)),
        }
    }
}

impl SplitEnumeratorImpl {
    pub async fn list_splits(&mut self) -> Result<Vec<SplitImpl>> {
        match self {
            SplitEnumeratorImpl::Kafka(k) => k
                .list_splits()
                .await
                .map(|ss| ss.into_iter().map(SplitImpl::Kafka).collect_vec()),
            SplitEnumeratorImpl::Pulsar(p) => p
                .list_splits()
                .await
                .map(|ss| ss.into_iter().map(SplitImpl::Pulsar).collect_vec()),
            SplitEnumeratorImpl::Kinesis(k) => k
                .list_splits()
                .await
                .map(|ss| ss.into_iter().map(SplitImpl::Kinesis).collect_vec()),
        }
    }

    pub fn create(properties: &AnyhowProperties) -> Result<SplitEnumeratorImpl> {
        let source_type = properties.get(UPSTREAM_SOURCE_KEY)?;
        match source_type.as_str() {
            KAFKA_SOURCE => KafkaSplitEnumerator::new(properties).map(SplitEnumeratorImpl::Kafka),
            PULSAR_SOURCE => {
                PulsarSplitEnumerator::new(properties).map(SplitEnumeratorImpl::Pulsar)
            }
            KINESIS_SOURCE => todo!(),
            _ => Err(anyhow!("unsupported source type: {}", source_type)),
        }
    }
}

pub async fn new_connector(
    config: Properties,
    state: Option<ConnectorState>,
) -> Result<Box<dyn SourceReader + Send + Sync>> {
    let upstream_type = config.get(UPSTREAM_SOURCE_KEY)?;
    let connector: Box<dyn SourceReader + Send + Sync> = match upstream_type.as_str() {
        KAFKA_SOURCE => Box::new(KafkaSplitReader::new(config, state).await?),
        KINESIS_SOURCE => Box::new(KinesisSplitReader::new(config, state).await?),
        _other => {
            todo!()
        }
    };
    Ok(connector)
}
