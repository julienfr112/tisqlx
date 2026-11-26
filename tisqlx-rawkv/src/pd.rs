//! The PD client: finds the leader, then asks it for regions and stores.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::grpc::{Channels, unary};
use crate::proto::{metapb, pdpb};
use crate::region::RegionInfo;

const GET_MEMBERS: &str = "/pdpb.PD/GetMembers";
const GET_REGION: &str = "/pdpb.PD/GetRegion";
const GET_STORE: &str = "/pdpb.PD/GetStore";

/// Attempts per PD call; between them the leader is looked up again.
const ATTEMPTS: u32 = 4;

#[derive(Clone, Debug)]
struct Leader {
    addr: String,
    cluster_id: u64,
}

#[derive(Debug)]
pub(crate) struct Pd {
    endpoints: Vec<String>,
    channels: Arc<Channels>,
    leader: Mutex<Option<Leader>>,
}

/// PD's answer to anything: a header that may carry an error.
trait HasHeader {
    fn header(&self) -> Option<&pdpb::ResponseHeader>;
}

macro_rules! has_header {
    ($($t:ty),*) => {$(
        impl HasHeader for $t {
            fn header(&self) -> Option<&pdpb::ResponseHeader> {
                self.header.as_ref()
            }
        }
    )*};
}
has_header!(pdpb::GetMembersResponse, pdpb::GetRegionResponse, pdpb::GetStoreResponse);

fn header_error(resp: &impl HasHeader) -> Option<Error> {
    let err = resp.header()?.error.as_ref()?;
    (err.r#type != 0).then(|| Error::Pd { kind: err.r#type, message: err.message.clone() })
}

impl Pd {
    /// Connects to the first endpoint that names a leader.
    pub async fn connect(endpoints: Vec<String>, channels: Arc<Channels>) -> Result<Self> {
        if endpoints.is_empty() {
            return Err(Error::InvalidArgument("no PD endpoint".into()));
        }
        let pd = Self { endpoints, channels, leader: Mutex::new(None) };
        pd.leader().await?;
        Ok(pd)
    }

    async fn leader(&self) -> Result<Leader> {
        if let Some(l) = self.leader.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return Ok(l);
        }
        let mut last = String::from("no endpoint tried");
        for ep in &self.endpoints {
            match self.members(ep).await {
                Ok(resp) => {
                    let cluster_id = resp.header.as_ref().map_or(0, |h| h.cluster_id);
                    match resp.leader.and_then(|m| m.client_urls.into_iter().next()) {
                        Some(addr) => {
                            let leader = Leader { addr, cluster_id };
                            log::debug!("PD leader {} (cluster {cluster_id})", leader.addr);
                            *self.leader.lock().unwrap_or_else(|e| e.into_inner()) = Some(leader.clone());
                            return Ok(leader);
                        }
                        None => last = format!("{ep} names no leader"),
                    }
                }
                Err(e) => last = e.to_string(),
            }
        }
        Err(Error::NoPdLeader { endpoints: self.endpoints.clone(), last })
    }

    async fn members(&self, ep: &str) -> Result<pdpb::GetMembersResponse> {
        let ch = self.channels.get(ep)?;
        let resp: pdpb::GetMembersResponse =
            unary(ch, ep, GET_MEMBERS, pdpb::GetMembersRequest { header: Some(Default::default()) }).await?;
        match header_error(&resp) {
            Some(e) => Err(e),
            None => Ok(resp),
        }
    }

    fn forget_leader(&self, addr: &str) {
        let mut leader = self.leader.lock().unwrap_or_else(|e| e.into_inner());
        if leader.as_ref().is_some_and(|l| l.addr == addr) {
            *leader = None;
        }
        self.channels.forget(addr);
    }

    /// Sends a request to the leader, looking the leader up again after a failure (PD
    /// followers refuse requests, and the leader may have moved).
    async fn call<Req, Resp>(&self, path: &'static str, build: impl Fn(pdpb::RequestHeader) -> Req) -> Result<Resp>
    where
        Req: prost::Message + Send + Sync + 'static,
        Resp: prost::Message + Default + Send + Sync + HasHeader + 'static,
    {
        let mut last = None;
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(50 << attempt)).await;
            }
            let leader = match self.leader().await {
                Ok(l) => l,
                Err(e) => {
                    last = Some(e);
                    continue;
                }
            };
            let header = pdpb::RequestHeader { cluster_id: leader.cluster_id, sender_id: 0 };
            let ch = self.channels.get(&leader.addr)?;
            match unary::<Req, Resp>(ch, &leader.addr, path, build(header)).await {
                Ok(resp) => {
                    return match header_error(&resp) {
                        Some(e) => Err(e),
                        None => Ok(resp),
                    };
                }
                Err(e) => {
                    log::debug!("PD {path} at {} failed: {e}", leader.addr);
                    self.forget_leader(&leader.addr);
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| Error::NoPdLeader { endpoints: self.endpoints.clone(), last: String::new() }))
    }

    /// The region holding `key`, with its leader if PD knows it.
    pub async fn get_region(&self, key: &[u8]) -> Result<RegionInfo> {
        let resp: pdpb::GetRegionResponse = self
            .call(GET_REGION, |header| pdpb::GetRegionRequest { header: Some(header), region_key: key.to_vec() })
            .await?;
        let meta = resp.region.ok_or_else(|| Error::NotFoundInPd(format!("region for key {key:?}")))?;
        Ok(RegionInfo { meta, leader: resp.leader.filter(|p| p.id != 0) })
    }

    pub async fn get_store(&self, store_id: u64) -> Result<metapb::Store> {
        let resp: pdpb::GetStoreResponse =
            self.call(GET_STORE, |header| pdpb::GetStoreRequest { header: Some(header), store_id }).await?;
        resp.store.ok_or_else(|| Error::NotFoundInPd(format!("store {store_id}")))
    }
}
