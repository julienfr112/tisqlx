//! Unary gRPC calls over cached channels, with the hand-written messages of [`crate::proto`].

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use tonic::codec::ProstCodec;
use tonic::codegen::http::uri::PathAndQuery;
use tonic::transport::{Channel, Endpoint};

use crate::error::{Error, Result};

/// One HTTP/2 channel per address, shared by every request to it.
#[derive(Debug)]
pub(crate) struct Channels {
    map: Mutex<HashMap<String, Channel>>,
    connect_timeout: Duration,
    timeout: Duration,
}

impl Channels {
    pub fn new(connect_timeout: Duration, timeout: Duration) -> Self {
        Self { map: Mutex::new(HashMap::new()), connect_timeout, timeout }
    }

    /// `addr` is `host:port` (TiKV store addresses) or a URL (PD client URLs).
    pub fn get(&self, addr: &str) -> Result<Channel> {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ch) = map.get(addr) {
            return Ok(ch.clone());
        }
        let uri = if addr.contains("://") { addr.to_owned() } else { format!("http://{addr}") };
        if uri.starts_with("https://") {
            return Err(Error::InvalidAddress { addr: addr.to_owned(), message: "TLS is not supported yet".into() });
        }
        let endpoint = Endpoint::from_shared(uri)
            .map_err(|e| Error::InvalidAddress { addr: addr.to_owned(), message: e.to_string() })?
            .connect_timeout(self.connect_timeout)
            .timeout(self.timeout)
            .tcp_nodelay(true)
            .http2_keep_alive_interval(Duration::from_secs(10))
            .keep_alive_timeout(Duration::from_secs(3));
        // Lazy: the first request connects, and a broken connection reconnects by itself.
        let ch = endpoint.connect_lazy();
        map.insert(addr.to_owned(), ch.clone());
        Ok(ch)
    }

    /// Drops the channel to `addr`, so the next request opens a new one.
    pub fn forget(&self, addr: &str) {
        self.map.lock().unwrap_or_else(|e| e.into_inner()).remove(addr);
    }
}

/// Sends one request to `path` (`/package.Service/Method`).
pub(crate) async fn unary<Req, Resp>(ch: Channel, addr: &str, path: &'static str, req: Req) -> Result<Resp>
where
    Req: prost::Message + Send + Sync + 'static,
    Resp: prost::Message + Default + Send + Sync + 'static,
{
    let grpc_err = |status| Error::Grpc { addr: addr.to_owned(), status: Box::new(status) };
    let mut grpc = tonic::client::Grpc::new(ch)
        // Scans and batch gets can exceed tonic's 4 MiB default; TiKV bounds them itself.
        .max_decoding_message_size(usize::MAX)
        .max_encoding_message_size(usize::MAX);
    grpc.ready().await.map_err(|e| grpc_err(tonic::Status::unavailable(e.to_string())))?;
    grpc.unary(tonic::Request::new(req), PathAndQuery::from_static(path), ProstCodec::<Req, Resp>::default())
        .await
        .map(tonic::Response::into_inner)
        .map_err(grpc_err)
}
