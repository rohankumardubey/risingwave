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

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use etcd_client::{Client, ConnectOptions, Error, GetOptions};
use risingwave_pb::meta::MetaLeaderInfo;
use tokio::sync::oneshot;
use tokio::time;
use tokio_stream::StreamExt;

use crate::MetaResult;

const META_ELECTION_KEY: &str = "__meta_election";

#[async_trait::async_trait]
pub trait ElectionClient: Send + Sync + 'static {
    async fn run_once(&self, ttl: i64) -> MetaResult<()>;
    async fn leader(&self) -> MetaResult<Option<MetaLeaderInfo>>;
    async fn get_members(&self) -> MetaResult<Vec<(String, i64, bool)>>;
    fn is_leader(&self) -> bool;
}

pub struct EtcdElectionClient {
    pub client: Client,
    pub is_leader: AtomicBool,
    pub id: String,
}

impl EtcdElectionClient {
    fn value_to_address(value: &[u8]) -> String {
        String::from_utf8_lossy(value).to_string()
    }
}

#[async_trait::async_trait]
impl ElectionClient for EtcdElectionClient {
    fn is_leader(&self) -> bool {
        self.is_leader.load(Ordering::Relaxed)
    }

    async fn leader(&self) -> MetaResult<Option<MetaLeaderInfo>> {
        if self.is_leader.load(Ordering::Relaxed) {
            return Ok(Some(MetaLeaderInfo {
                node_address: self.id.clone(),
                lease_id: 0,
            }));
        }

        let mut election_client = self.client.election_client();
        let leader = election_client.leader(META_ELECTION_KEY).await;

        let leader = match leader {
            Ok(leader) => Ok(Some(leader)),
            Err(Error::GRpcStatus(e)) if e.message() == "election: no leader" => Ok(None),
            Err(e) => Err(e),
        }?;

        Ok(leader.map(|mut leader| {
            let leader_kv = leader.take_kv().unwrap();

            MetaLeaderInfo {
                node_address: String::from_utf8_lossy(leader_kv.value()).to_string(),
                lease_id: leader_kv.lease() as u64,
            }
        }))
    }

    async fn run_once(&self, ttl: i64) -> MetaResult<()> {
        let mut lease_client = self.client.lease_client();
        let mut election_client = self.client.election_client();

        self.is_leader.store(false, Ordering::Relaxed);

        let leader_resp = election_client.leader(META_ELECTION_KEY).await;

        let mut lease_id = match leader_resp.map(|mut resp| resp.take_kv()) {
            // leader exists
            Ok(Some(leader_kv)) if leader_kv.value() == self.id.as_bytes() => Ok(leader_kv.lease()),

            // leader kv not exists (may not happen)
            Ok(_) => lease_client.grant(ttl, None).await.map(|resp| resp.id()),

            // no leader
            Err(Error::GRpcStatus(e)) if e.message() == "election: no leader" => {
                lease_client.grant(ttl, None).await.map(|resp| resp.id())
            }

            // connection error
            Err(e) => Err(e),
        }?;

        // try keep alive
        let (mut keeper, mut resp_stream) = lease_client.keep_alive(lease_id).await.unwrap();
        let _resp = keeper.keep_alive().await?;
        let resp = resp_stream.message().await?;
        if let Some(resp) = resp {
            if resp.ttl() == 0 {
                tracing::info!("lease {} expired or revoked, re-granting", lease_id);
                // renew lease_id
                lease_id = lease_client.grant(ttl, None).await.map(|resp| resp.id())?;
                tracing::info!("lease {} re-granted", lease_id);
            }
        }

        let (keep_alive_fail_tx, keep_alive_fail_rx) = oneshot::channel();

        let mut lease_client = self.client.lease_client();

        let handle = tokio::spawn(async move {
            let (mut keeper, mut resp_stream) = lease_client.keep_alive(lease_id).await.unwrap();

            let mut ticker = time::interval(Duration::from_secs(1));

            loop {
                ticker.tick().await;
                let resp = keeper.keep_alive().await;
                if let Err(err) = resp {
                    tracing::error!("keep alive for lease {} failed {}", lease_id, err);
                    keep_alive_fail_tx.send(()).unwrap();
                    break;
                }

                if let Some(resp) = resp_stream.message().await.unwrap() {
                    if resp.ttl() <= 0 {
                        tracing::warn!("lease expired or revoked {}", lease_id);
                        keep_alive_fail_tx.send(()).unwrap();
                        break;
                    }
                }
            }
            tracing::info!("keep alive loop for lease {} stopped", lease_id);
        });

        let resp = election_client
            .campaign(META_ELECTION_KEY, self.id.as_bytes().to_vec(), lease_id)
            .await?;

        tracing::info!(
            "client {} wins election {}",
            self.id,
            resp.leader().unwrap().name_str().unwrap()
        );

        self.is_leader.store(true, Ordering::Relaxed);

        let _keep_alive_resp = keep_alive_fail_rx.await;

        tracing::warn!("client {} lost leadership", self.id);

        self.is_leader.store(false, Ordering::Relaxed);

        handle.abort();

        Ok(())
    }

    async fn get_members(&self) -> MetaResult<Vec<(String, i64, bool)>> {
        let mut client = self.client.kv_client();
        let keys = client
            .get(META_ELECTION_KEY, Some(GetOptions::new().with_prefix()))
            .await?;
        let mut kvs = keys.kvs().to_vec();

        kvs.sort_by(|a, b| {
            a.create_revision()
                .partial_cmp(&b.create_revision())
                .unwrap()
        });

        Ok(kvs
            .into_iter()
            .enumerate()
            .map(|(i, kv)| {
                (
                    String::from_utf8_lossy(kv.value()).to_string(),
                    kv.lease(),
                    i == 0,
                )
            })
            .collect())
    }
}
impl EtcdElectionClient {
    pub(crate) async fn new(
        endpoints: Vec<String>,
        options: Option<ConnectOptions>,
        id: String,
    ) -> MetaResult<Self> {
        let client = Client::connect(&endpoints, options.clone()).await?;

        Ok(Self {
            client,
            is_leader: AtomicBool::new(false),
            id,
        })
    }
}
