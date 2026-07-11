use crate::backend::BackendKind;
use crate::backend_pool::BackendPool;
use crate::message::{RpcId, RpcMessage};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::time::Instant;
use url::Url;

/// TTL for deduplicating client-visible `window/showMessage` notifications
/// about repeated backend-creation failures for the same venv (issue #26).
/// Deliberately a plain const rather than an env var: server-side logs are
/// unaffected by this window, so there is no operator-facing behavior to
/// tune.
const NOTIFICATION_DEDUP_TTL: Duration = Duration::from_secs(30);

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

/// State for a fan-out request (dispatched to all backends, results merged)
pub struct PendingFanout {
    /// Original client request ID
    pub client_request_id: RpcId,
    /// Number of backends we are still waiting for
    pub expected_count: usize,
    /// Collected results from successful backends
    pub results: Vec<serde_json::Value>,
    /// Maps `proxy_id` → (`venv_path`, session) for each sub-request
    pub sub_requests: HashMap<RpcId, (PathBuf, u64)>,
    /// Deadline for the fan-out (partial results returned after this).
    /// None means no timeout (wait forever).
    pub deadline: Option<Instant>,
    /// Whether we already sent a window/showMessage notification for this fan-out
    pub notified: bool,
    /// Venv paths of backends that failed or timed out
    pub failed_backends: Vec<PathBuf>,
    /// Original client request (needed to build error response if all fail)
    pub client_request: RpcMessage,
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
    /// Which LSP backend to use
    pub backend_kind: BackendKind,

    /// Git toplevel (search boundary, cached on first retrieval)
    pub git_toplevel: Option<PathBuf>,

    /// Initialize message from Claude Code (reused for backend initialization)
    pub client_initialize: Option<RpcMessage>,

    /// Open documents
    pub open_documents: HashMap<Url, OpenDocument>,

    /// Pending requests (client → backend)
    pub pending_requests: HashMap<RpcId, PendingRequest>,

    /// Pending backend requests (backend → client, keyed by `proxy_id`)
    /// Maps `proxy_id` → `PendingBackendRequest` to route client responses back to correct backend
    pub pending_backend_requests: HashMap<RpcId, PendingBackendRequest>,

    /// Next proxy ID for server→client requests (monotonically increasing to avoid collisions)
    pub next_proxy_request_id: i64,

    /// Backend pool
    pub pool: BackendPool,

    /// Pending fan-out requests (keyed by client request ID)
    pub pending_fanouts: HashMap<RpcId, PendingFanout>,

    /// Venvs already checked for a gitignored project root (checked at most
    /// once per venv per proxy lifetime, even if the backend is later
    /// TTL-evicted and respawned).
    pub ignore_checked_venvs: HashSet<PathBuf>,

    /// Last time a client-visible backend-failure notification was sent for
    /// a venv, keyed by venv path. Used to dedupe `window/showMessage`
    /// spam when the same venv keeps failing to spawn (issue #26).
    pub venv_error_notified_at: HashMap<PathBuf, Instant>,
}

impl ProxyState {
    pub fn new(
        backend_kind: BackendKind,
        max_backends: usize,
        backend_ttl: Option<Duration>,
    ) -> Self {
        Self {
            backend_kind,
            git_toplevel: None,
            client_initialize: None,
            open_documents: HashMap::new(),
            pending_requests: HashMap::new(),
            pending_backend_requests: HashMap::new(),
            next_proxy_request_id: -1, // Use negative IDs to avoid collision with client IDs
            pool: BackendPool::new(max_backends, backend_ttl),
            pending_fanouts: HashMap::new(),
            ignore_checked_venvs: HashSet::new(),
            venv_error_notified_at: HashMap::new(),
        }
    }

    /// Allocate a new proxy request ID for server→client requests.
    /// Uses negative numbers (decrementing) to avoid collision with client-originated IDs (positive).
    pub fn alloc_proxy_request_id(&mut self) -> RpcId {
        let id = self.next_proxy_request_id;
        self.next_proxy_request_id -= 1;
        RpcId::Number(id)
    }

    /// Return the nearest fan-out deadline among all pending fan-outs.
    /// Returns None if no fan-outs are pending.
    pub fn nearest_fanout_deadline(&self) -> Option<Instant> {
        self.pending_fanouts
            .values()
            .filter_map(|f| f.deadline)
            .min()
    }

    /// Returns whether a client-visible `window/showMessage` notification
    /// should be sent for a backend failure in `venv_path` at `now`.
    /// Records `now` as the last-notified time when it returns `true`.
    /// Repeated failures for the same venv within `NOTIFICATION_DEDUP_TTL`
    /// return `false` — callers must still log every failure server-side,
    /// since this only governs the client-visible notification.
    pub fn should_notify_backend_failure(&mut self, venv_path: &Path, now: Instant) -> bool {
        let recently_notified = self
            .venv_error_notified_at
            .get(venv_path)
            .is_some_and(|last| now.duration_since(*last) < NOTIFICATION_DEDUP_TTL);
        if recently_notified {
            return false;
        }
        self.venv_error_notified_at
            .insert(venv_path.to_path_buf(), now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::BackendKind;

    fn test_state() -> ProxyState {
        ProxyState::new(BackendKind::Pyright, 4, None)
    }

    #[test]
    fn should_notify_backend_failure_dedupes_within_ttl() {
        let mut state = test_state();
        let venv = PathBuf::from("/tmp/project/.venv");
        let t0 = Instant::now();

        // First failure: no prior notification, so it should notify.
        assert!(state.should_notify_backend_failure(&venv, t0));

        // A second failure a few seconds later, still within the TTL,
        // must be suppressed.
        assert!(!state.should_notify_backend_failure(&venv, t0 + Duration::from_secs(10)));

        // A different venv is tracked independently.
        let other_venv = PathBuf::from("/tmp/other/.venv");
        assert!(state.should_notify_backend_failure(&other_venv, t0 + Duration::from_secs(10)));
    }

    #[test]
    fn should_notify_backend_failure_renotifies_after_ttl_expires() {
        let mut state = test_state();
        let venv = PathBuf::from("/tmp/project/.venv");
        let t0 = Instant::now();

        assert!(state.should_notify_backend_failure(&venv, t0));

        // Just past the TTL: a new failure should notify again.
        let after_ttl = t0 + NOTIFICATION_DEDUP_TTL + Duration::from_millis(1);
        assert!(state.should_notify_backend_failure(&venv, after_ttl));
    }
}
