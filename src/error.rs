use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ProxyError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Invalid message: {0}")]
    InvalidMessage(String),

    #[error("Backend error: {0}")]
    Backend(#[from] BackendError),

    #[error("Framing error: {0}")]
    Framing(#[from] FramingError),

    #[error("Venv error: {0}")]
    Venv(#[from] VenvError),

    #[error("Backend pool is full and no backend is evictable for {0}")]
    PoolBusy(PathBuf),
}

#[derive(Error, Debug)]
pub enum BackendError {
    #[error("Failed to spawn backend: {0}")]
    SpawnFailed(#[from] std::io::Error),

    #[error("Backend communication error: {0}")]
    Communication(#[from] FramingError),

    #[error("Initialize timeout after {0}s")]
    InitializeTimeout(u64),

    #[error("Initialize failed: {0}")]
    InitializeFailed(String),

    #[error("Initialize response error: {0}")]
    InitializeResponseError(String),
}

#[derive(Error, Debug)]
pub enum FramingError {
    #[error("Missing Content-Length header")]
    MissingContentLength,

    #[error("Invalid Content-Length value")]
    InvalidContentLength,

    #[error("Duplicate Content-Length header")]
    DuplicateContentLength,

    #[error("Content-Length {actual} exceeds maximum frame size of {limit} bytes")]
    ContentLengthTooLarge { limit: usize, actual: usize },

    #[error("Header line exceeds maximum length of {limit} bytes")]
    HeaderLineTooLong { limit: usize },

    #[error("Header block exceeds maximum total size of {limit} bytes")]
    HeaderBlockTooLarge { limit: usize },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Error, Debug)]
pub enum VenvError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
