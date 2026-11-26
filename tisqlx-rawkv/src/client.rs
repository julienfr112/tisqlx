//! [`RawClient`]: RawKV operations routed to the region leaders.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::stream::{self, StreamExt, TryStreamExt};

use crate::error::{Error, Result};
use crate::grpc::{Channels, unary};
use crate::pd::Pd;
use crate::proto::{errorpb, kvrpcpb, metapb};
use crate::region::{RegionCache, RegionInfo, clamp_end, walk_done};

/// The most pairs one [`RawClient::scan`] returns (this client's cap, the same as TiKV's Go
/// and Rust clients).
pub const MAX_SCAN_LIMIT: u32 = 10240;

/// Keys per request when a batch is split (also split by [`BATCH_BYTES`]).
const BATCH_KEYS: usize = 2048;
/// Bytes of keys and values per request, well below TiKV's `raft-entry-max-size` (8 MiB).
const BATCH_BYTES: usize = 2 * 1024 * 1024;
/// Requests to different regions in flight at once, per batch.
const BATCH_CONCURRENCY: usize = 16;

/// Client settings. `Config::default()` suits a local cluster and most deployments.
#[derive(Clone, Debug)]
pub struct Config {
    /// Timeout of one gRPC request (default 5 s).
    pub timeout: Duration,
    /// Timeout of a TCP connection to PD or TiKV (default 3 s).
    pub connect_timeout: Duration,
    /// How long an operation keeps retrying region errors (leader changes, splits, a busy or
    /// unreachable store) before failing with [`Error::RetriesExhausted`] (default 20 s).
    pub retry_deadline: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(3),
            retry_deadline: Duration::from_secs(20),
        }
    }
}

/// The column family raw operations use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ColumnFamily {
    /// `default`: the right choice unless you know otherwise.
    #[default]
    Default,
    /// `lock`.
    Lock,
    /// `write`.
    Write,
}

impl ColumnFamily {
    fn name(self) -> &'static str {
        match self {
            ColumnFamily::Default => "", // TiKV reads an empty name as `default`
            ColumnFamily::Lock => "lock",
            ColumnFamily::Write => "write",
        }
    }
}

/// A key and its value.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KvPair {
    /// The key.
    pub key: Vec<u8>,
    /// The value (empty from a `key_only` scan).
    pub value: Vec<u8>,
}

impl KvPair {
    /// A pair.
    pub fn new(key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> Self {
        Self { key: key.into(), value: value.into() }
    }
}

impl<K: Into<Vec<u8>>, V: Into<Vec<u8>>> From<(K, V)> for KvPair {
    fn from((k, v): (K, V)) -> Self {
        Self::new(k, v)
    }
}

/// The outcome of [`RawClient::compare_and_swap`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CasResult {
    /// True if the value was swapped in.
    pub swapped: bool,
    /// The value before the call (`None`: the key did not exist).
    pub previous: Option<Vec<u8>>,
}

/// A RawKV client for TiKV.
///
/// Cheap to clone: clones share the connections and the region cache. Each operation goes to
/// the leader of the region holding its key, found through PD and cached; region errors
/// (leader changes, splits, merges, a busy store) refresh the cache and retry, up to
/// [`Config::retry_deadline`].
///
/// Raw writes are not transactional: a batch spread over several regions is several
/// independent writes, and some may land while others fail.
///
/// **TiKV API version.** This client speaks API V1, the default. A TiKV with
/// `storage.enable-ttl = true` (V1TTL) also accepts TTLs; that setting is only allowed on a
/// RawKV-only cluster. On a cluster that also runs TiDB, raw keys share the key space with
/// TiDB's data (keys starting with `t` and `m`): keep raw keys under a prefix of your own, and
/// never scan or delete an unbounded range there.
#[derive(Clone, Debug)]
pub struct RawClient {
    inner: Arc<Inner>,
    cf: ColumnFamily,
    atomic: bool,
}

#[derive(Debug)]
struct Inner {
    pd: Pd,
    channels: Arc<Channels>,
    regions: Mutex<RegionCache>,
    stores: Mutex<HashMap<u64, String>>,
    config: Config,
}

/// What one attempt at a region gave.
enum Attempt<T> {
    Done(T),
    /// Retry after refreshing the route; `Some(ZERO)` retries at once (a new leader is known).
    Retry {
        wait: Option<Duration>,
        reason: String,
    },
}

/// Retry pacing shared by the requests of one operation.
struct Backoff {
    start: Instant,
    deadline: Duration,
    attempt: u32,
    immediate: u32,
}

impl Backoff {
    fn new(deadline: Duration) -> Self {
        Self { start: Instant::now(), deadline, attempt: 0, immediate: 0 }
    }

    fn retried(&self) -> bool {
        self.attempt > 0 || self.immediate > 0
    }

    async fn wait(&mut self, hint: Option<Duration>, reason: String) -> Result<()> {
        let elapsed = self.start.elapsed();
        if elapsed >= self.deadline {
            return Err(Error::RetriesExhausted { elapsed, last: reason });
        }
        // A known new leader is retried at once, but only a few times in a row.
        if hint == Some(Duration::ZERO) && self.immediate < 4 {
            self.immediate += 1;
            log::debug!("RawKV retry now: {reason}");
            return Ok(());
        }
        let exp = Duration::from_millis(2u64 << self.attempt.min(9)); // 4 ms .. ~1 s
        let delay = hint.unwrap_or_default().max(exp).min(self.deadline - elapsed);
        self.attempt += 1;
        log::debug!("RawKV retry in {delay:?}: {reason}");
        tokio::time::sleep(delay).await;
        Ok(())
    }
}

/// A raw request: its gRPC method and where the routing context goes.
trait RawRequest: prost::Message + Clone + Send + Sync + 'static {
    type Resp: RawResponse;
    const PATH: &'static str;
    fn set_context(&mut self, ctx: kvrpcpb::Context);
}

trait RawResponse: prost::Message + Default + Send + Sync + 'static {
    fn take_region_error(&mut self) -> Option<errorpb::Error>;
    /// The `error` string, for the responses that have one.
    fn server_error(&self) -> &str {
        ""
    }
}

macro_rules! raw_request {
    ($($req:ty => $resp:ty, $path:literal, $has_error:tt;)*) => {$(
        impl RawRequest for $req {
            type Resp = $resp;
            const PATH: &'static str = $path;
            fn set_context(&mut self, ctx: kvrpcpb::Context) {
                self.context = Some(ctx);
            }
        }
        impl RawResponse for $resp {
            fn take_region_error(&mut self) -> Option<errorpb::Error> {
                self.region_error.take()
            }
            raw_request!(@error $has_error);
        }
    )*};
    (@error yes) => {
        fn server_error(&self) -> &str {
            &self.error
        }
    };
    (@error no) => {};
}

raw_request! {
    kvrpcpb::RawGetRequest => kvrpcpb::RawGetResponse, "/tikvpb.Tikv/RawGet", yes;
    kvrpcpb::RawBatchGetRequest => kvrpcpb::RawBatchGetResponse, "/tikvpb.Tikv/RawBatchGet", no;
    kvrpcpb::RawPutRequest => kvrpcpb::RawPutResponse, "/tikvpb.Tikv/RawPut", yes;
    kvrpcpb::RawBatchPutRequest => kvrpcpb::RawBatchPutResponse, "/tikvpb.Tikv/RawBatchPut", yes;
    kvrpcpb::RawDeleteRequest => kvrpcpb::RawDeleteResponse, "/tikvpb.Tikv/RawDelete", yes;
    kvrpcpb::RawBatchDeleteRequest => kvrpcpb::RawBatchDeleteResponse, "/tikvpb.Tikv/RawBatchDelete", yes;
    kvrpcpb::RawDeleteRangeRequest => kvrpcpb::RawDeleteRangeResponse, "/tikvpb.Tikv/RawDeleteRange", yes;
    kvrpcpb::RawScanRequest => kvrpcpb::RawScanResponse, "/tikvpb.Tikv/RawScan", no;
    kvrpcpb::RawGetKeyTtlRequest => kvrpcpb::RawGetKeyTtlResponse, "/tikvpb.Tikv/RawGetKeyTTL", yes;
    kvrpcpb::RawCasRequest => kvrpcpb::RawCasResponse, "/tikvpb.Tikv/RawCompareAndSwap", yes;
    kvrpcpb::SplitRegionRequest => kvrpcpb::SplitRegionResponse, "/tikvpb.Tikv/SplitRegion", no;
}

fn key_error(e: &kvrpcpb::KeyError) -> Error {
    Error::Server(if e.abort.is_empty() { e.retryable.clone() } else { e.abort.clone() })
}

/// Splits sorted items into requests of at most [`BATCH_KEYS`] items and about
/// [`BATCH_BYTES`] bytes.
/// Sorts pairs by key and keeps the last pair given for each key. Requests to one region go
/// out concurrently and are retried, so two pairs with one key must never be sent apart.
fn last_per_key(mut items: Vec<(KvPair, u64)>) -> Vec<(KvPair, u64)> {
    items.reverse();
    items.sort_by(|a, b| a.0.key.cmp(&b.0.key)); // stable: the last given comes first
    items.dedup_by(|later, first| later.0.key == first.0.key);
    items
}

fn chunks<T>(items: Vec<T>, size: impl Fn(&T) -> usize) -> Vec<Vec<T>> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    let mut bytes = 0;
    for item in items {
        let n = size(&item);
        if !cur.is_empty() && (cur.len() == BATCH_KEYS || bytes + n > BATCH_BYTES) {
            out.push(std::mem::take(&mut cur));
            bytes = 0;
        }
        bytes += n;
        cur.push(item);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

impl RawClient {
    /// Connects through PD, e.g. `RawClient::connect(["127.0.0.1:2379"])`.
    ///
    /// Several endpoints may be given; the first one that answers names the PD leader.
    /// Connections to TiKV are opened when first needed.
    pub async fn connect<S: Into<String>>(pd_endpoints: impl IntoIterator<Item = S>) -> Result<Self> {
        Self::connect_with(pd_endpoints, Config::default()).await
    }

    /// [`connect`](Self::connect) with explicit settings.
    pub async fn connect_with<S: Into<String>>(
        pd_endpoints: impl IntoIterator<Item = S>,
        config: Config,
    ) -> Result<Self> {
        let channels = Arc::new(Channels::new(config.connect_timeout, config.timeout));
        let endpoints = pd_endpoints.into_iter().map(Into::into).collect();
        let pd = Pd::connect(endpoints, channels.clone()).await?;
        let inner = Inner {
            pd,
            channels,
            regions: Mutex::new(RegionCache::default()),
            stores: Mutex::new(HashMap::new()),
            config,
        };
        Ok(Self { inner: Arc::new(inner), cf: ColumnFamily::Default, atomic: false })
    }

    /// A client using another column family (sharing this one's connections and cache).
    pub fn with_cf(&self, cf: ColumnFamily) -> Self {
        Self { cf, ..self.clone() }
    }

    /// A client in *atomic mode*, needed by [`compare_and_swap`](Self::compare_and_swap).
    ///
    /// TiKV only makes a CAS atomic against writes that are themselves sent "for CAS": in
    /// atomic mode every put and delete is, which makes them slower. Every client writing the
    /// keys a CAS touches must be in atomic mode, or the CAS can lose their updates.
    /// [`delete_range`](Self::delete_range) is refused in atomic mode.
    pub fn with_atomic_for_cas(&self) -> Self {
        Self { atomic: true, ..self.clone() }
    }

    // ---- reads ----

    /// The value of `key`, or `None`.
    pub async fn get(&self, key: impl Into<Vec<u8>>) -> Result<Option<Vec<u8>>> {
        let key = key.into();
        let req = kvrpcpb::RawGetRequest { key: key.clone(), cf: self.cf().into(), ..Default::default() };
        let (_, resp) = self.single(&key, |_| req.clone()).await?;
        Ok((!resp.not_found).then_some(resp.value))
    }

    /// The pairs of the keys that exist, in key order (duplicates are read once).
    pub async fn batch_get<K: Into<Vec<u8>>>(&self, keys: impl IntoIterator<Item = K>) -> Result<Vec<KvPair>> {
        let mut keys: Vec<Vec<u8>> = keys.into_iter().map(Into::into).collect();
        keys.sort();
        keys.dedup();
        let cf = self.cf();
        let resps = self
            .batch(
                keys,
                |k| k,
                |k| k.len(),
                |keys| kvrpcpb::RawBatchGetRequest { keys, cf: cf.into(), ..Default::default() },
            )
            .await?;
        let mut out = Vec::new();
        for resp in resps {
            for p in resp.pairs {
                if let Some(e) = &p.error {
                    return Err(key_error(e));
                }
                out.push(KvPair { key: p.key, value: p.value });
            }
        }
        out.sort_unstable_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    /// Up to `limit` pairs in `[start, end)`, in key order. An empty `end` means no upper
    /// bound. `limit` is at most [`MAX_SCAN_LIMIT`].
    pub async fn scan(&self, start: impl Into<Vec<u8>>, end: impl Into<Vec<u8>>, limit: u32) -> Result<Vec<KvPair>> {
        self.scan_inner(start.into(), end.into(), limit, false).await
    }

    /// [`scan`](Self::scan) returning keys only (each pair's value is empty).
    pub async fn scan_keys(
        &self,
        start: impl Into<Vec<u8>>,
        end: impl Into<Vec<u8>>,
        limit: u32,
    ) -> Result<Vec<Vec<u8>>> {
        Ok(self.scan_inner(start.into(), end.into(), limit, true).await?.into_iter().map(|p| p.key).collect())
    }

    async fn scan_inner(&self, start: Vec<u8>, end: Vec<u8>, limit: u32, key_only: bool) -> Result<Vec<KvPair>> {
        if limit > MAX_SCAN_LIMIT {
            return Err(Error::InvalidArgument(format!("scan limit {limit} exceeds {MAX_SCAN_LIMIT}")));
        }
        if !end.is_empty() && start >= end {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let mut cur = start;
        while (out.len() as u32) < limit {
            let remaining = limit - out.len() as u32;
            let (region, resp) = self
                .single(&cur, |region| kvrpcpb::RawScanRequest {
                    start_key: cur.clone(),
                    end_key: clamp_end(region.end_key(), &end),
                    limit: remaining,
                    key_only,
                    cf: self.cf().into(),
                    ..Default::default()
                })
                .await?;
            for p in resp.kvs {
                if let Some(e) = &p.error {
                    return Err(key_error(e));
                }
                out.push(KvPair { key: p.key, value: p.value });
            }
            if walk_done(region.end_key(), &end) {
                break;
            }
            cur = region.end_key().to_vec();
        }
        out.truncate(limit as usize);
        Ok(out)
    }

    /// The remaining time to live of `key` in seconds: `Some(0)` if it has none, `None` if
    /// the key does not exist. Needs `storage.enable-ttl` on TiKV.
    pub async fn get_key_ttl_secs(&self, key: impl Into<Vec<u8>>) -> Result<Option<u64>> {
        let key = key.into();
        let req = kvrpcpb::RawGetKeyTtlRequest { key: key.clone(), cf: self.cf().into(), ..Default::default() };
        let (_, resp) = self.single(&key, |_| req.clone()).await?;
        Ok((!resp.not_found).then_some(resp.ttl))
    }

    // ---- writes ----

    /// Sets `key` to `value`.
    pub async fn put(&self, key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> Result<()> {
        self.put_inner(key.into(), value.into(), 0).await
    }

    /// Sets `key` to `value`, expiring after `ttl_secs` seconds (0: never). Needs
    /// `storage.enable-ttl` on TiKV (API V1TTL); a plain V1 TiKV rejects it.
    pub async fn put_with_ttl(&self, key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>, ttl_secs: u64) -> Result<()> {
        self.put_inner(key.into(), value.into(), ttl_secs).await
    }

    async fn put_inner(&self, key: Vec<u8>, value: Vec<u8>, ttl: u64) -> Result<()> {
        let req = kvrpcpb::RawPutRequest {
            key: key.clone(),
            value,
            cf: self.cf().into(),
            ttl,
            for_cas: self.atomic,
            ..Default::default()
        };
        self.single(&key, |_| req.clone()).await.map(drop)
    }

    /// Writes several pairs. Each region's share is one atomic write; the batch as a whole
    /// is not (see [`RawClient`]).
    pub async fn batch_put<P: Into<KvPair>>(&self, pairs: impl IntoIterator<Item = P>) -> Result<()> {
        self.batch_put_inner(pairs.into_iter().map(|p| (p.into(), 0)).collect()).await
    }

    /// [`batch_put`](Self::batch_put) with one TTL in seconds for every pair (0: never).
    pub async fn batch_put_with_ttl<P: Into<KvPair>>(
        &self,
        pairs: impl IntoIterator<Item = P>,
        ttl_secs: u64,
    ) -> Result<()> {
        self.batch_put_inner(pairs.into_iter().map(|p| (p.into(), ttl_secs)).collect()).await
    }

    async fn batch_put_inner(&self, items: Vec<(KvPair, u64)>) -> Result<()> {
        let items = last_per_key(items);
        let (cf, for_cas) = (self.cf(), self.atomic);
        self.batch(
            items,
            |(p, _)| &p.key,
            |(p, _)| p.key.len() + p.value.len(),
            |items| {
                let ttls = if items.iter().all(|(_, t)| *t == 0) {
                    Vec::new()
                } else {
                    items.iter().map(|(_, t)| *t).collect()
                };
                kvrpcpb::RawBatchPutRequest {
                    pairs: items
                        .into_iter()
                        .map(|(p, _)| kvrpcpb::KvPair { error: None, key: p.key, value: p.value })
                        .collect(),
                    cf: cf.into(),
                    for_cas,
                    ttls,
                    ..Default::default()
                }
            },
        )
        .await
        .map(drop)
    }

    /// Deletes `key` (nothing happens if it does not exist).
    pub async fn delete(&self, key: impl Into<Vec<u8>>) -> Result<()> {
        let key = key.into();
        let req = kvrpcpb::RawDeleteRequest {
            key: key.clone(),
            cf: self.cf().into(),
            for_cas: self.atomic,
            ..Default::default()
        };
        self.single(&key, |_| req.clone()).await.map(drop)
    }

    /// Deletes several keys.
    pub async fn batch_delete<K: Into<Vec<u8>>>(&self, keys: impl IntoIterator<Item = K>) -> Result<()> {
        let mut keys: Vec<Vec<u8>> = keys.into_iter().map(Into::into).collect();
        keys.sort();
        keys.dedup();
        let (cf, for_cas) = (self.cf(), self.atomic);
        self.batch(
            keys,
            |k| k,
            |k| k.len(),
            |keys| kvrpcpb::RawBatchDeleteRequest { keys, cf: cf.into(), for_cas, ..Default::default() },
        )
        .await
        .map(drop)
    }

    /// Deletes every key in `[start, end)`. `end` must not be empty: an unbounded delete is
    /// too easy to get wrong.
    pub async fn delete_range(&self, start: impl Into<Vec<u8>>, end: impl Into<Vec<u8>>) -> Result<()> {
        let (start, end) = (start.into(), end.into());
        if self.atomic {
            return Err(Error::InvalidArgument("delete_range is not available in atomic mode".into()));
        }
        if end.is_empty() {
            return Err(Error::InvalidArgument("delete_range needs a non-empty end key".into()));
        }
        let mut cur = start;
        while cur < end {
            let (region, _) = self
                .single(&cur, |region| kvrpcpb::RawDeleteRangeRequest {
                    start_key: cur.clone(),
                    end_key: clamp_end(region.end_key(), &end),
                    cf: self.cf().into(),
                    ..Default::default()
                })
                .await?;
            if walk_done(region.end_key(), &end) {
                break;
            }
            cur = region.end_key().to_vec();
        }
        Ok(())
    }

    /// Sets `key` to `new` if its current value is `expected` (`None`: the key must not
    /// exist). Only on a client in [atomic mode](Self::with_atomic_for_cas).
    ///
    /// A CAS that times out and is retried may report `swapped: false` although the first
    /// attempt swapped: on a timeout, read the key to find out.
    pub async fn compare_and_swap(
        &self,
        key: impl Into<Vec<u8>>,
        expected: Option<Vec<u8>>,
        new: impl Into<Vec<u8>>,
    ) -> Result<CasResult> {
        if !self.atomic {
            return Err(Error::InvalidArgument(
                "compare_and_swap needs a client in atomic mode (RawClient::with_atomic_for_cas)".into(),
            ));
        }
        let key = key.into();
        let req = kvrpcpb::RawCasRequest {
            key: key.clone(),
            value: new.into(),
            previous_not_exist: expected.is_none(),
            previous_value: expected.unwrap_or_default(),
            cf: self.cf().into(),
            ..Default::default()
        };
        let (_, resp) = self.single(&key, |_| req.clone()).await?;
        Ok(CasResult { swapped: resp.succeed, previous: (!resp.previous_not_exist).then_some(resp.previous_value) })
    }

    /// Splits the regions holding `keys` so that each key starts a region (keys already at a
    /// region boundary are skipped). Useful to pre-split a key range before a bulk
    /// load, so the writes spread over several regions from the start.
    pub async fn split_regions<K: Into<Vec<u8>>>(&self, keys: impl IntoIterator<Item = K>) -> Result<()> {
        let mut keys: Vec<Vec<u8>> = keys.into_iter().map(Into::into).collect();
        keys.sort();
        keys.dedup();
        // TiKV answers a split at an existing boundary with a region error, not a no-op.
        let mut todo = Vec::with_capacity(keys.len());
        for key in keys {
            if self.locate(&key).await?.start_key() != key.as_slice() {
                todo.push(key);
            }
        }
        let resps = self
            .batch(
                todo,
                |k| k,
                |k| k.len(),
                |split_keys| kvrpcpb::SplitRegionRequest { split_keys, is_raw_kv: true, ..Default::default() },
            )
            .await?;
        let mut cache = self.inner.regions.lock().unwrap_or_else(|e| e.into_inner());
        for resp in resps {
            if let Some(e) = resp.errors.first() {
                return Err(key_error(e));
            }
            for meta in resp.regions {
                cache.insert(RegionInfo { meta, leader: None });
            }
        }
        Ok(())
    }

    // ---- routing ----

    fn cf(&self) -> &'static str {
        self.cf.name()
    }

    async fn locate(&self, key: &[u8]) -> Result<RegionInfo> {
        if let Some(r) = self.inner.regions.lock().unwrap_or_else(|e| e.into_inner()).locate(key) {
            return Ok(r.clone());
        }
        let region = self.inner.pd.get_region(key).await?;
        self.inner.regions.lock().unwrap_or_else(|e| e.into_inner()).insert(region.clone());
        Ok(region)
    }

    async fn store_addr(&self, store_id: u64) -> Result<String> {
        if let Some(a) = self.inner.stores.lock().unwrap_or_else(|e| e.into_inner()).get(&store_id) {
            return Ok(a.clone());
        }
        let store = self.inner.pd.get_store(store_id).await?;
        if store.state == metapb::STORE_TOMBSTONE {
            return Err(Error::NotFoundInPd(format!("live store {store_id} (it is a tombstone)")));
        }
        self.inner.stores.lock().unwrap_or_else(|e| e.into_inner()).insert(store_id, store.address.clone());
        Ok(store.address)
    }

    fn invalidate_region(&self, region_id: u64) {
        self.inner.regions.lock().unwrap_or_else(|e| e.into_inner()).invalidate(region_id);
    }

    fn forget_store(&self, store_id: u64) {
        if let Some(addr) = self.inner.stores.lock().unwrap_or_else(|e| e.into_inner()).remove(&store_id) {
            self.inner.channels.forget(&addr);
        }
    }

    /// One request to one region; a region error updates the cache and asks for a retry.
    async fn send_once<R: RawRequest>(
        &self,
        region: &RegionInfo,
        mut req: R,
        is_retry: bool,
    ) -> Result<Attempt<R::Resp>> {
        let Some(peer) = region.target_peer() else {
            self.invalidate_region(region.id());
            return Ok(Attempt::Retry { wait: None, reason: format!("region {} has no peer", region.id()) });
        };
        let addr = self.store_addr(peer.store_id).await?;
        req.set_context(kvrpcpb::Context {
            region_id: region.id(),
            region_epoch: region.meta.region_epoch.clone(),
            peer: Some(peer.clone()),
            is_retry_request: is_retry,
            request_source: "tisqlx-rawkv".into(),
            ..Default::default()
        });
        let ch = self.inner.channels.get(&addr)?;
        let mut resp = match unary::<R, R::Resp>(ch, &addr, R::PATH, req).await {
            Ok(resp) => resp,
            Err(Error::Grpc { status, .. }) if is_transport_failure(&status) => {
                // The store may be down or moved: route again from PD.
                self.invalidate_region(region.id());
                self.forget_store(peer.store_id);
                return Ok(Attempt::Retry { wait: None, reason: format!("store {addr}: {status}") });
            }
            Err(e) => return Err(e),
        };
        if let Some(err) = resp.take_region_error() {
            return self.on_region_error(region, &peer, err).map(|(wait, reason)| Attempt::Retry { wait, reason });
        }
        match resp.server_error() {
            "" => Ok(Attempt::Done(resp)),
            e => Err(Error::Server(e.to_owned())),
        }
    }

    /// Updates the cache after a region error; returns the wait hint and a description.
    fn on_region_error(
        &self,
        region: &RegionInfo,
        peer: &metapb::Peer,
        e: errorpb::Error,
    ) -> Result<(Option<Duration>, String)> {
        let id = region.id();
        let reason = format!("region {id}: {}", e.message);
        if let Some(nl) = e.not_leader {
            return Ok(match nl.leader.filter(|l| l.id != 0) {
                Some(leader) => {
                    let known = self.inner.regions.lock().unwrap_or_else(|e| e.into_inner()).update_leader(id, leader);
                    (known.then_some(Duration::ZERO), reason)
                }
                // An election is in progress.
                None => {
                    self.invalidate_region(id);
                    (None, reason)
                }
            });
        }
        if let Some(enm) = e.epoch_not_match {
            let mut cache = self.inner.regions.lock().unwrap_or_else(|e| e.into_inner());
            cache.invalidate(id);
            let fresh = !enm.current_regions.is_empty();
            for meta in enm.current_regions {
                // The peer that answered is still the leader of the regions it reports.
                let leader = meta.peers.iter().find(|p| p.store_id == peer.store_id).cloned();
                cache.insert(RegionInfo { meta, leader });
            }
            return Ok((fresh.then_some(Duration::ZERO), reason));
        }
        if let Some(busy) = e.server_is_busy {
            return Ok((Some(Duration::from_millis(busy.backoff_ms)), format!("{reason} (busy: {})", busy.reason)));
        }
        if e.store_not_match.is_some() {
            self.forget_store(peer.store_id);
        }
        if let Some(big) = e.raft_entry_too_large {
            return Err(Error::Server(format!("raft entry too large ({} bytes) in region {id}", big.entry_size)));
        }
        // Region not found, key not in region, store mismatch, and anything else: ask PD again.
        self.invalidate_region(id);
        Ok((None, reason))
    }

    /// A request routed by `key`, retried on region errors. `build` makes the request for the
    /// region currently holding `key` (a range request clamps itself to it).
    async fn single<R: RawRequest>(
        &self,
        key: &[u8],
        build: impl Fn(&RegionInfo) -> R,
    ) -> Result<(RegionInfo, R::Resp)> {
        let mut backoff = Backoff::new(self.inner.config.retry_deadline);
        loop {
            let region = self.locate(key).await?;
            match self.send_once(&region, build(&region), backoff.retried()).await? {
                Attempt::Done(resp) => return Ok((region, resp)),
                Attempt::Retry { wait, reason } => backoff.wait(wait, reason).await?,
            }
        }
    }

    /// Requests for items sorted by key, grouped by region and chunked; the items of a group
    /// that hit a region error are grouped again and resent.
    async fn batch<T, R>(
        &self,
        items: Vec<T>,
        key_of: impl Fn(&T) -> &Vec<u8>,
        size_of: impl Fn(&T) -> usize,
        build: impl Fn(Vec<T>) -> R,
    ) -> Result<Vec<R::Resp>>
    where
        T: Clone,
        R: RawRequest,
    {
        let mut backoff = Backoff::new(self.inner.config.retry_deadline);
        let mut todo = items;
        let mut out = Vec::new();
        while !todo.is_empty() {
            // Group by region (items are sorted, so each region's items are contiguous).
            let mut groups: Vec<(RegionInfo, Vec<T>)> = Vec::new();
            for item in todo.drain(..) {
                match groups.last_mut() {
                    Some((region, group)) if region.contains(key_of(&item)) => group.push(item),
                    _ => {
                        let region = self.locate(key_of(&item)).await?;
                        groups.push((region, vec![item]));
                    }
                }
            }
            let requests = groups
                .into_iter()
                .flat_map(|(region, group)| chunks(group, &size_of).into_iter().map(move |c| (region.clone(), c)));
            let retried = backoff.retried();
            let results: Vec<(Vec<T>, Attempt<R::Resp>)> = stream::iter(requests)
                .map(|(region, chunk)| {
                    let req = build(chunk.clone());
                    async move { self.send_once(&region, req, retried).await.map(|a| (chunk, a)) }
                })
                .buffer_unordered(BATCH_CONCURRENCY)
                .try_collect()
                .await?;
            let mut hint = Some(Duration::ZERO);
            let mut last = String::new();
            for (chunk, attempt) in results {
                match attempt {
                    Attempt::Done(resp) => out.push(resp),
                    Attempt::Retry { wait, reason } => {
                        todo.extend(chunk);
                        // Wait as long as the slowest hint asks.
                        hint = match (hint, wait) {
                            (Some(a), Some(b)) => Some(a.max(b)),
                            _ => None,
                        };
                        last = reason;
                    }
                }
            }
            if !todo.is_empty() {
                todo.sort_by(|a, b| key_of(a).cmp(key_of(b)));
                backoff.wait(hint, last).await?;
            }
        }
        Ok(out)
    }
}

/// The request never reached a working TiKV: refused connection, timeout, broken stream.
fn is_transport_failure(status: &tonic::Status) -> bool {
    matches!(status.code(), tonic::Code::Unavailable | tonic::Code::DeadlineExceeded | tonic::Code::Cancelled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_put_keeps_the_last_value_of_a_key() {
        let items = vec![
            (KvPair::new("b", "1"), 0),
            (KvPair::new("a", "1"), 5),
            (KvPair::new("b", "2"), 0),
            (KvPair::new("b", "3"), 7),
            (KvPair::new("c", "1"), 0),
        ];
        assert_eq!(
            last_per_key(items),
            vec![(KvPair::new("a", "1"), 5), (KvPair::new("b", "3"), 7), (KvPair::new("c", "1"), 0)]
        );
    }

    #[test]
    fn chunks_respect_count_and_bytes() {
        let items: Vec<usize> = (0..5000).collect();
        let c = chunks(items, |_| 1);
        assert_eq!(c.iter().map(Vec::len).collect::<Vec<_>>(), [2048, 2048, 904]);
        let big = chunks(vec![BATCH_BYTES / 2 + 1; 3], |n| *n);
        assert_eq!(big.len(), 3, "two items over the byte budget never share a request");
        let huge = chunks(vec![BATCH_BYTES * 2], |n| *n);
        assert_eq!(huge.len(), 1, "an item bigger than the budget still goes, alone");
        assert!(chunks(Vec::<usize>::new(), |n| *n).is_empty());
    }

    #[test]
    fn cf_names() {
        assert_eq!(ColumnFamily::Default.name(), "");
        assert_eq!(ColumnFamily::Lock.name(), "lock");
        assert_eq!(ColumnFamily::Write.name(), "write");
    }

    #[tokio::test]
    async fn backoff_gives_up_at_the_deadline() {
        let mut b = Backoff::new(Duration::from_millis(30));
        for _ in 0..4 {
            b.wait(Some(Duration::ZERO), "leader moved".into()).await.unwrap();
        }
        assert_eq!(b.attempt, 0, "immediate retries do not sleep");
        let err = loop {
            if let Err(e) = b.wait(None, "busy".into()).await {
                break e;
            }
        };
        assert!(matches!(err, Error::RetriesExhausted { .. }), "{err}");
    }
}
