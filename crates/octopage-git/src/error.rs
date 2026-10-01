use std::time::Duration;

use crate::ObjectId;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A ref did not hold the expected value: another writer moved it first.
    /// This is the normal outcome of a lost race, not a failure of the transport.
    #[error("compare-and-swap failed on {refname}: {reason}")]
    Conflict { refname: String, reason: String },

    /// The remote refused the push for a reason other than a ref conflict.
    #[error("push rejected: {0}")]
    Rejected(String),

    /// The push may or may not have been applied (the connection failed after it was sent).
    /// Read the ref to find out; commits are deterministic, so the caller can recognise its own.
    #[error("push outcome unknown ({0}); read the ref to learn whether it landed")]
    PushOutcomeUnknown(String),

    #[error("rate limited by GitHub{}", retry_after.map(|d| format!("; retry after {}s", d.as_secs())).unwrap_or_default())]
    RateLimited { retry_after: Option<Duration> },

    #[error("authentication failed (HTTP {status}): {message}")]
    Auth { status: u16, message: String },

    #[error("not found: {0}")]
    NotFound(String),

    #[error("HTTP {status} from {url}: {body}")]
    Http {
        status: u16,
        url: String,
        body: String,
    },

    #[error("network error: {0}")]
    Network(#[source] reqwest::Error),

    #[error("objects missing on the remote: {0:?}")]
    MissingObjects(Vec<ObjectId>),

    #[error("remote error: {0}")]
    Remote(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    /// Bytes did not match their id, or a SHA-1 collision attack was detected.
    #[error("integrity error: {0}")]
    Integrity(String),

    #[error("invalid input: {0}")]
    Invalid(String),
}

pub(crate) fn protocol(message: impl Into<String>) -> Error {
    Error::Protocol(message.into())
}

pub(crate) fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}
