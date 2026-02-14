use crate::backend_pool::BackendPool;
use crate::message::{RpcId, RpcMessage};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use url::Url;

/// Information about pending requests
#[derive(Debug, Clone)]
pub struct PendingRequest {
    /// Backend session this request was sent to
    pub backend_session: u64,
    /// Venv path of the backend this request was sent to
    pub venv_path: PathBuf,
}

/// Information about a pending server→client request (backend → proxy → client)
/// Used to route client responses back to the correct backend.
#[derive(Debug, Clone)]
pub struct PendingBackendRequest {
    /// Original backend-assigned ID (to restore when forwarding response back)
    pub original_id: RpcId,
    /// Venv path of the originating backend
    pub venv_path: PathBuf,
    /// Session of the originating backend
    pub session: u64,
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

    /// Pending requests (client → backend)
    pub pending_requests: HashMap<RpcId, PendingRequest>,

    /// Pending backend requests (backend → client, keyed by proxy_id)
    /// Maps proxy_id → PendingBackendRequest to route client responses back to correct backend
    pub pending_backend_requests: HashMap<RpcId, PendingBackendRequest>,

    /// Next proxy ID for server→client requests (monotonically increasing to avoid collisions)
    pub next_proxy_request_id: i64,

    /// Backend pool
    pub pool: BackendPool,
}

impl ProxyState {
    pub fn new(max_backends: usize, backend_ttl: Option<Duration>) -> Self {
        Self {
            git_toplevel: None,
            client_initialize: None,
            open_documents: HashMap::new(),
            pending_requests: HashMap::new(),
            pending_backend_requests: HashMap::new(),
            next_proxy_request_id: -1, // Use negative IDs to avoid collision with client IDs
            pool: BackendPool::new(max_backends, backend_ttl),
        }
    }

    /// Allocate a new proxy request ID for server→client requests.
    /// Uses negative numbers (decrementing) to avoid collision with client-originated IDs (positive).
    pub fn alloc_proxy_request_id(&mut self) -> RpcId {
        let id = self.next_proxy_request_id;
        self.next_proxy_request_id -= 1;
        RpcId::Number(id)
    }
}
