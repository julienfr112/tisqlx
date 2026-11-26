//! The subset of kvproto (release-8.5) this client sends and reads, written by hand.
//!
//! Field numbers and types are copied from `metapb.proto`, `errorpb.proto`, `pdpb.proto` and
//! `kvrpcpb.proto`. Protobuf skips fields a message does not declare, so a trimmed message
//! still decodes what the server sends, and needs no `protoc` at build time. Fields left out
//! here are never needed by RawKV; add them with their exact upstream number.

#![allow(missing_docs)]

pub mod metapb {
    #[derive(Clone, PartialEq, Eq, prost::Message)]
    pub struct RegionEpoch {
        #[prost(uint64, tag = "1")]
        pub conf_ver: u64,
        #[prost(uint64, tag = "2")]
        pub version: u64,
    }

    #[derive(Clone, PartialEq, Eq, prost::Message)]
    pub struct Peer {
        #[prost(uint64, tag = "1")]
        pub id: u64,
        #[prost(uint64, tag = "2")]
        pub store_id: u64,
        /// `PeerRole`: 0 Voter, 1 Learner, 2 IncomingVoter, 3 DemotingVoter.
        #[prost(int32, tag = "3")]
        pub role: i32,
        #[prost(bool, tag = "4")]
        pub is_witness: bool,
    }

    #[derive(Clone, PartialEq, Eq, prost::Message)]
    pub struct Region {
        #[prost(uint64, tag = "1")]
        pub id: u64,
        #[prost(bytes = "vec", tag = "2")]
        pub start_key: Vec<u8>,
        /// Empty means +∞.
        #[prost(bytes = "vec", tag = "3")]
        pub end_key: Vec<u8>,
        #[prost(message, optional, tag = "4")]
        pub region_epoch: Option<RegionEpoch>,
        #[prost(message, repeated, tag = "5")]
        pub peers: Vec<Peer>,
    }

    /// `StoreState`: Tombstone.
    pub const STORE_TOMBSTONE: i32 = 2;

    #[derive(Clone, PartialEq, Eq, prost::Message)]
    pub struct Store {
        #[prost(uint64, tag = "1")]
        pub id: u64,
        #[prost(string, tag = "2")]
        pub address: String,
        #[prost(int32, tag = "3")]
        pub state: i32,
    }
}

pub mod errorpb {
    use super::metapb;

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct NotLeader {
        #[prost(uint64, tag = "1")]
        pub region_id: u64,
        #[prost(message, optional, tag = "2")]
        pub leader: Option<metapb::Peer>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct RegionNotFound {
        #[prost(uint64, tag = "1")]
        pub region_id: u64,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct KeyNotInRegion {
        #[prost(bytes = "vec", tag = "1")]
        pub key: Vec<u8>,
        #[prost(uint64, tag = "2")]
        pub region_id: u64,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct EpochNotMatch {
        #[prost(message, repeated, tag = "1")]
        pub current_regions: Vec<metapb::Region>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ServerIsBusy {
        #[prost(string, tag = "1")]
        pub reason: String,
        #[prost(uint64, tag = "2")]
        pub backoff_ms: u64,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct StoreNotMatch {
        #[prost(uint64, tag = "1")]
        pub request_store_id: u64,
        #[prost(uint64, tag = "2")]
        pub actual_store_id: u64,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct RaftEntryTooLarge {
        #[prost(uint64, tag = "1")]
        pub region_id: u64,
        #[prost(uint64, tag = "2")]
        pub entry_size: u64,
    }

    /// A region error. Only the variants the client acts on are decoded; any other one still
    /// carries `message` and is retried after a backoff.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Error {
        #[prost(string, tag = "1")]
        pub message: String,
        #[prost(message, optional, tag = "2")]
        pub not_leader: Option<NotLeader>,
        #[prost(message, optional, tag = "3")]
        pub region_not_found: Option<RegionNotFound>,
        #[prost(message, optional, tag = "4")]
        pub key_not_in_region: Option<KeyNotInRegion>,
        #[prost(message, optional, tag = "5")]
        pub epoch_not_match: Option<EpochNotMatch>,
        #[prost(message, optional, tag = "6")]
        pub server_is_busy: Option<ServerIsBusy>,
        #[prost(message, optional, tag = "8")]
        pub store_not_match: Option<StoreNotMatch>,
        #[prost(message, optional, tag = "9")]
        pub raft_entry_too_large: Option<RaftEntryTooLarge>,
    }
}

pub mod pdpb {
    use super::metapb;

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct RequestHeader {
        #[prost(uint64, tag = "1")]
        pub cluster_id: u64,
        #[prost(uint64, tag = "2")]
        pub sender_id: u64,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Error {
        /// `ErrorType`: 0 OK, 6 REGION_NOT_FOUND, ...
        #[prost(int32, tag = "1")]
        pub r#type: i32,
        #[prost(string, tag = "2")]
        pub message: String,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ResponseHeader {
        #[prost(uint64, tag = "1")]
        pub cluster_id: u64,
        #[prost(message, optional, tag = "2")]
        pub error: Option<Error>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Member {
        #[prost(string, tag = "1")]
        pub name: String,
        #[prost(uint64, tag = "2")]
        pub member_id: u64,
        #[prost(string, repeated, tag = "4")]
        pub client_urls: Vec<String>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct GetMembersRequest {
        #[prost(message, optional, tag = "1")]
        pub header: Option<RequestHeader>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct GetMembersResponse {
        #[prost(message, optional, tag = "1")]
        pub header: Option<ResponseHeader>,
        #[prost(message, repeated, tag = "2")]
        pub members: Vec<Member>,
        #[prost(message, optional, tag = "3")]
        pub leader: Option<Member>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct GetRegionRequest {
        #[prost(message, optional, tag = "1")]
        pub header: Option<RequestHeader>,
        #[prost(bytes = "vec", tag = "2")]
        pub region_key: Vec<u8>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct GetRegionResponse {
        #[prost(message, optional, tag = "1")]
        pub header: Option<ResponseHeader>,
        #[prost(message, optional, tag = "2")]
        pub region: Option<metapb::Region>,
        #[prost(message, optional, tag = "3")]
        pub leader: Option<metapb::Peer>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct GetStoreRequest {
        #[prost(message, optional, tag = "1")]
        pub header: Option<RequestHeader>,
        #[prost(uint64, tag = "2")]
        pub store_id: u64,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct GetStoreResponse {
        #[prost(message, optional, tag = "1")]
        pub header: Option<ResponseHeader>,
        #[prost(message, optional, tag = "2")]
        pub store: Option<metapb::Store>,
    }
}

pub mod kvrpcpb {
    use super::{errorpb, metapb};

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Context {
        #[prost(uint64, tag = "1")]
        pub region_id: u64,
        #[prost(message, optional, tag = "2")]
        pub region_epoch: Option<metapb::RegionEpoch>,
        #[prost(message, optional, tag = "3")]
        pub peer: Option<metapb::Peer>,
        #[prost(bool, tag = "20")]
        pub is_retry_request: bool,
        /// `APIVersion`: 0 V1 (also what a V1TTL server expects from clients), 2 V2.
        #[prost(int32, tag = "21")]
        pub api_version: i32,
        #[prost(string, tag = "24")]
        pub request_source: String,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct KeyError {
        #[prost(string, tag = "2")]
        pub retryable: String,
        #[prost(string, tag = "3")]
        pub abort: String,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct KvPair {
        #[prost(message, optional, tag = "1")]
        pub error: Option<KeyError>,
        #[prost(bytes = "vec", tag = "2")]
        pub key: Vec<u8>,
        #[prost(bytes = "vec", tag = "3")]
        pub value: Vec<u8>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct RawGetRequest {
        #[prost(message, optional, tag = "1")]
        pub context: Option<Context>,
        #[prost(bytes = "vec", tag = "2")]
        pub key: Vec<u8>,
        #[prost(string, tag = "3")]
        pub cf: String,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct RawGetResponse {
        #[prost(message, optional, tag = "1")]
        pub region_error: Option<errorpb::Error>,
        #[prost(string, tag = "2")]
        pub error: String,
        #[prost(bytes = "vec", tag = "3")]
        pub value: Vec<u8>,
        #[prost(bool, tag = "4")]
        pub not_found: bool,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct RawBatchGetRequest {
        #[prost(message, optional, tag = "1")]
        pub context: Option<Context>,
        #[prost(bytes = "vec", repeated, tag = "2")]
        pub keys: Vec<Vec<u8>>,
        #[prost(string, tag = "3")]
        pub cf: String,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct RawBatchGetResponse {
        #[prost(message, optional, tag = "1")]
        pub region_error: Option<errorpb::Error>,
        #[prost(message, repeated, tag = "2")]
        pub pairs: Vec<KvPair>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct RawPutRequest {
        #[prost(message, optional, tag = "1")]
        pub context: Option<Context>,
        #[prost(bytes = "vec", tag = "2")]
        pub key: Vec<u8>,
        #[prost(bytes = "vec", tag = "3")]
        pub value: Vec<u8>,
        #[prost(string, tag = "4")]
        pub cf: String,
        #[prost(uint64, tag = "5")]
        pub ttl: u64,
        #[prost(bool, tag = "6")]
        pub for_cas: bool,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct RawPutResponse {
        #[prost(message, optional, tag = "1")]
        pub region_error: Option<errorpb::Error>,
        #[prost(string, tag = "2")]
        pub error: String,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct RawBatchPutRequest {
        #[prost(message, optional, tag = "1")]
        pub context: Option<Context>,
        #[prost(message, repeated, tag = "2")]
        pub pairs: Vec<KvPair>,
        #[prost(string, tag = "3")]
        pub cf: String,
        #[prost(bool, tag = "5")]
        pub for_cas: bool,
        /// One per pair, or empty for no TTL.
        #[prost(uint64, repeated, tag = "6")]
        pub ttls: Vec<u64>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct RawBatchPutResponse {
        #[prost(message, optional, tag = "1")]
        pub region_error: Option<errorpb::Error>,
        #[prost(string, tag = "2")]
        pub error: String,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct RawDeleteRequest {
        #[prost(message, optional, tag = "1")]
        pub context: Option<Context>,
        #[prost(bytes = "vec", tag = "2")]
        pub key: Vec<u8>,
        #[prost(string, tag = "3")]
        pub cf: String,
        #[prost(bool, tag = "4")]
        pub for_cas: bool,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct RawDeleteResponse {
        #[prost(message, optional, tag = "1")]
        pub region_error: Option<errorpb::Error>,
        #[prost(string, tag = "2")]
        pub error: String,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct RawBatchDeleteRequest {
        #[prost(message, optional, tag = "1")]
        pub context: Option<Context>,
        #[prost(bytes = "vec", repeated, tag = "2")]
        pub keys: Vec<Vec<u8>>,
        #[prost(string, tag = "3")]
        pub cf: String,
        #[prost(bool, tag = "4")]
        pub for_cas: bool,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct RawBatchDeleteResponse {
        #[prost(message, optional, tag = "1")]
        pub region_error: Option<errorpb::Error>,
        #[prost(string, tag = "2")]
        pub error: String,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct RawDeleteRangeRequest {
        #[prost(message, optional, tag = "1")]
        pub context: Option<Context>,
        #[prost(bytes = "vec", tag = "2")]
        pub start_key: Vec<u8>,
        #[prost(bytes = "vec", tag = "3")]
        pub end_key: Vec<u8>,
        #[prost(string, tag = "4")]
        pub cf: String,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct RawDeleteRangeResponse {
        #[prost(message, optional, tag = "1")]
        pub region_error: Option<errorpb::Error>,
        #[prost(string, tag = "2")]
        pub error: String,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct RawScanRequest {
        #[prost(message, optional, tag = "1")]
        pub context: Option<Context>,
        #[prost(bytes = "vec", tag = "2")]
        pub start_key: Vec<u8>,
        #[prost(uint32, tag = "3")]
        pub limit: u32,
        #[prost(bool, tag = "4")]
        pub key_only: bool,
        #[prost(string, tag = "5")]
        pub cf: String,
        #[prost(bool, tag = "6")]
        pub reverse: bool,
        #[prost(bytes = "vec", tag = "7")]
        pub end_key: Vec<u8>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct RawScanResponse {
        #[prost(message, optional, tag = "1")]
        pub region_error: Option<errorpb::Error>,
        #[prost(message, repeated, tag = "2")]
        pub kvs: Vec<KvPair>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct RawGetKeyTtlRequest {
        #[prost(message, optional, tag = "1")]
        pub context: Option<Context>,
        #[prost(bytes = "vec", tag = "2")]
        pub key: Vec<u8>,
        #[prost(string, tag = "3")]
        pub cf: String,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct RawGetKeyTtlResponse {
        #[prost(message, optional, tag = "1")]
        pub region_error: Option<errorpb::Error>,
        #[prost(string, tag = "2")]
        pub error: String,
        #[prost(uint64, tag = "3")]
        pub ttl: u64,
        #[prost(bool, tag = "4")]
        pub not_found: bool,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct RawCasRequest {
        #[prost(message, optional, tag = "1")]
        pub context: Option<Context>,
        #[prost(bytes = "vec", tag = "2")]
        pub key: Vec<u8>,
        #[prost(bytes = "vec", tag = "3")]
        pub value: Vec<u8>,
        #[prost(bool, tag = "4")]
        pub previous_not_exist: bool,
        #[prost(bytes = "vec", tag = "5")]
        pub previous_value: Vec<u8>,
        #[prost(string, tag = "6")]
        pub cf: String,
        #[prost(uint64, tag = "7")]
        pub ttl: u64,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct RawCasResponse {
        #[prost(message, optional, tag = "1")]
        pub region_error: Option<errorpb::Error>,
        #[prost(string, tag = "2")]
        pub error: String,
        #[prost(bool, tag = "3")]
        pub succeed: bool,
        #[prost(bool, tag = "4")]
        pub previous_not_exist: bool,
        #[prost(bytes = "vec", tag = "5")]
        pub previous_value: Vec<u8>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct SplitRegionRequest {
        #[prost(message, optional, tag = "1")]
        pub context: Option<Context>,
        #[prost(bytes = "vec", repeated, tag = "3")]
        pub split_keys: Vec<Vec<u8>>,
        #[prost(bool, tag = "4")]
        pub is_raw_kv: bool,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct SplitRegionResponse {
        #[prost(message, optional, tag = "1")]
        pub region_error: Option<errorpb::Error>,
        #[prost(message, repeated, tag = "4")]
        pub regions: Vec<metapb::Region>,
        #[prost(message, repeated, tag = "100")]
        pub errors: Vec<KeyError>,
    }
}
