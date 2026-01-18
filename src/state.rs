use crate::message::{RpcId, RpcMessage};
use std::collections::HashMap;
use std::path::PathBuf;
use url::Url;

/// Information about pending requests
#[derive(Debug, Clone)]
pub struct PendingRequest {
    /// Backend session this request was sent to
    pub backend_session: u64,
}

/// Open document
#[derive(Debug, Clone)]
pub struct OpenDocument {
    pub language_id: String,
    pub version: i32,
    pub text: String,
    pub venv: Option<PathBuf>,
}

/// State held by proxy
pub struct ProxyState {
    /// Git toplevel (search boundary, cached on first retrieval)
    pub git_toplevel: Option<PathBuf>,

    /// Initialize message from Claude Code (reused for backend initialization)
    pub client_initialize: Option<RpcMessage>,

    /// Open documents
    pub open_documents: HashMap<Url, OpenDocument>,

    /// Backend restart generation (for logging and conflict avoidance)
    pub backend_session: u64,

    /// Pending requests (with generation)
    pub pending_requests: HashMap<RpcId, PendingRequest>,
}

impl ProxyState {
    pub fn new() -> Self {
        Self {
            git_toplevel: None,
            client_initialize: None,
            open_documents: HashMap::new(),
            backend_session: 0,
            pending_requests: HashMap::new(),
        }
    }
}

impl Default for ProxyState {
    fn default() -> Self {
        Self::new()
    }
}
