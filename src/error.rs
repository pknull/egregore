//! Error types — identity, feed integrity, crypto, storage, and peer communication.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum EgreError {
    #[error("identity not found at {path}")]
    IdentityNotFound { path: String },

    #[error("invalid keypair: {reason}")]
    InvalidKeypair { reason: String },

    #[error("signature verification failed")]
    SignatureInvalid,

    #[error("feed integrity violation: {reason}")]
    FeedIntegrity { reason: String },

    #[error("duplicate message: author={author} sequence={sequence}")]
    DuplicateMessage { author: String, sequence: u64 },

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("crypto error: {reason}")]
    Crypto { reason: String },

    #[error("handshake failed: {reason}")]
    HandshakeFailed { reason: String },

    #[error("peer error: {reason}")]
    Peer { reason: String },

    #[error("config error: {reason}")]
    Config { reason: String },

    #[error("schema error: {reason}")]
    Schema { reason: String },
}

pub type Result<T> = std::result::Result<T, EgreError>;
