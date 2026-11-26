//! Errors of the RawKV client.

use std::time::Duration;

/// A RawKV error.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A gRPC call failed: the server answered with a non-OK status, or the connection broke.
    #[error("gRPC error from {addr}: {status}")]
    Grpc {
        /// The PD or TiKV address.
        addr: String,
        /// The status (`Unavailable` when the connection itself failed).
        status: Box<tonic::Status>,
    },
    /// An address could not be turned into a gRPC endpoint.
    #[error("invalid address {addr:?}: {message}")]
    InvalidAddress {
        /// The address as given.
        addr: String,
        /// Why it was refused.
        message: String,
    },
    /// No PD endpoint answered, or none named a leader.
    #[error("no PD leader reachable through {endpoints:?}: {last}")]
    NoPdLeader {
        /// The endpoints tried.
        endpoints: Vec<String>,
        /// The last failure.
        last: String,
    },
    /// PD answered with an error header.
    #[error("PD error (type {kind}): {message}")]
    Pd {
        /// PD's `ErrorType` number.
        kind: i32,
        /// PD's message.
        message: String,
    },
    /// PD knows no region for a key, or no store for an id.
    #[error("PD has no {0}")]
    NotFoundInPd(String),
    /// TiKV refused the request (the `error` string of a raw response, or a key error).
    ///
    /// For instance a TTL sent to a TiKV without `storage.enable-ttl`, or a CAS on a server
    /// that cannot do it.
    #[error("TiKV error: {0}")]
    Server(String),
    /// A request kept hitting region errors (leader changes, splits, a busy store) until the
    /// retry deadline passed.
    #[error("region error retries exhausted after {elapsed:?}: {last}")]
    RetriesExhausted {
        /// Time spent retrying.
        elapsed: Duration,
        /// The last region error.
        last: String,
    },
    /// The call itself is invalid, e.g. a CAS on a client not in atomic mode, or an empty key.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
}

impl Error {
    /// True for an error a later attempt may not hit: an unreachable server, a timeout,
    /// exhausted region retries.
    pub fn is_transient(&self) -> bool {
        match self {
            Error::Grpc { status, .. } => matches!(
                status.code(),
                tonic::Code::Unavailable | tonic::Code::DeadlineExceeded | tonic::Code::ResourceExhausted
            ),
            Error::NoPdLeader { .. } | Error::RetriesExhausted { .. } => true,
            _ => false,
        }
    }
}

/// `Result` with the RawKV [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;
