use crate::backend::{shutdown_fire_and_forget, BackendParts};
use crate::error::BackendError;
use crate::framing::{LspFrameReader, LspFrameWriter};
use crate::message::{RpcId, RpcMessage};
use crate::venv::VenvToken;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// Warmup state for a backend instance.
/// After spawning, backends need time to build their cross-file index.
/// During `Warming`, index-dependent requests are queued until the backend
/// signals readiness or the timeout expires (fail-open).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarmupState {
    Warming,
    Ready,
}

/// Maximum number of index-dependent requests queued per backend during
/// warmup. Not configurable: a fixed ceiling is enough to bound memory growth
/// without adding another env var to the surface.
pub const MAX_WARMUP_QUEUE_LEN: usize = 64;

/// Queue length at which `try_queue_warmup_request` logs a one-time
/// approaching-capacity warning.
const WARMUP_QUEUE_WARN_THRESHOLD: usize = MAX_WARMUP_QUEUE_LEN * 4 / 5;

/// Default warmup timeout; overridable via `TYPEMUX_CC_WARMUP_TIMEOUT` env var.
const DEFAULT_WARMUP_TIMEOUT: Duration = Duration::from_secs(2);

/// Returns the warmup timeout duration.
/// `TYPEMUX_CC_WARMUP_TIMEOUT=0` means warmup is disabled (immediate Ready).
///
/// The `.ok()` below only ever swallows an "unset" case: `validate_env_config`
/// runs at startup and aborts on an unparseable value, so by the time this is
/// called on the normal startup path, a set value is guaranteed to parse.
/// (`--doctor` skips that validation and calls this directly; it reports raw
/// invalid values itself instead of relying on this fallback.)
pub fn warmup_timeout() -> Duration {
    std::env::var("TYPEMUX_CC_WARMUP_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(DEFAULT_WARMUP_TIMEOUT, Duration::from_secs)
}

/// Default fan-out timeout; overridable via `TYPEMUX_CC_FANOUT_TIMEOUT` env var.
const DEFAULT_FANOUT_TIMEOUT: Duration = Duration::from_secs(5);

/// Returns the fan-out timeout duration for workspace/symbol and similar multi-backend requests.
/// `TYPEMUX_CC_FANOUT_TIMEOUT=0` means no timeout (wait forever).
///
/// See `warmup_timeout`'s doc comment: reachable only with an unset or
/// already-validated value on the normal startup path.
pub fn fanout_timeout() -> Duration {
    std::env::var("TYPEMUX_CC_FANOUT_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(DEFAULT_FANOUT_TIMEOUT, Duration::from_secs)
}

/// Default venv staleness check interval (debounce); overridable via
/// `TYPEMUX_CC_VENV_CHECK_INTERVAL` env var.
const DEFAULT_VENV_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// Returns the debounce interval for pooled-backend venv identity checks.
/// `TYPEMUX_CC_VENV_CHECK_INTERVAL=0` disables venv identity tracking entirely.
///
/// See `warmup_timeout`'s doc comment: reachable only with an unset or
/// already-validated value on the normal startup path.
pub fn venv_check_interval() -> Duration {
    std::env::var("TYPEMUX_CC_VENV_CHECK_INTERVAL")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(DEFAULT_VENV_CHECK_INTERVAL, Duration::from_secs)
}

/// Env vars validated by `validate_env_config`, backing the three accessors above.
const TIMEOUT_ENV_VARS: [&str; 3] = [
    "TYPEMUX_CC_WARMUP_TIMEOUT",
    "TYPEMUX_CC_FANOUT_TIMEOUT",
    "TYPEMUX_CC_VENV_CHECK_INTERVAL",
];

/// Validates the timeout/interval env vars above at startup, before the event
/// loop runs. A set-but-unparseable value (e.g. `TYPEMUX_CC_FANOUT_TIMEOUT=5s`)
/// is a configuration error, not something to silently default around, so this
/// returns an error naming the variable and its raw value instead of letting
/// the accessors swallow it via `.ok()`. Not called for `--doctor`, which
/// reports invalid values instead of aborting.
pub fn validate_env_config() -> Result<(), String> {
    for env_var in TIMEOUT_ENV_VARS {
        if let Ok(raw) = std::env::var(env_var) {
            if raw.parse::<u64>().is_err() {
                return Err(format!(
                    "invalid {env_var}={raw:?}: expected a non-negative integer number of seconds"
                ));
            }
        }
    }
    Ok(())
}

/// Message from a backend reader task
pub struct BackendMessage {
    pub venv_path: PathBuf,
    pub session: u64,
    pub result: Result<RpcMessage, BackendError>,
}

/// A single backend instance in the pool
pub struct BackendInstance {
    pub writer: LspFrameWriter<ChildStdin>,
    pub child: Child,
    pub venv_path: PathBuf,
    pub session: u64,
    pub last_used: Instant,
    pub reader_task: JoinHandle<()>,
    pub next_id: u64,
    pub warmup_state: WarmupState,
    pub warmup_deadline: Instant,
    pub warmup_queue: Vec<RpcMessage>,
    /// Identity token captured at spawn (`None` if capture raced a `.venv`
    /// rebuild; adopted lazily on the first successful staleness check).
    pub venv_token: Option<VenvToken>,
    /// Debounce marker for `check_pooled_venv_staleness`.
    pub last_venv_check: Instant,
    /// Set when `pyvenv.cfg` was first observed missing; cleared once it
    /// reappears. Used to bound how long a backend serves without a venv
    /// before being treated as removed.
    pub venv_missing_since: Option<Instant>,
}

impl BackendInstance {
    /// Create a `BackendInstance` from a split backend, spawning the reader task
    /// and computing the warmup state. Does NOT insert into the pool.
    pub fn from_parts(
        parts: BackendParts,
        venv_path: PathBuf,
        session: u64,
        msg_sender: mpsc::Sender<BackendMessage>,
        venv_token: Option<VenvToken>,
    ) -> Self {
        let reader_task = spawn_reader_task(parts.reader, msg_sender, venv_path.clone(), session);
        let timeout = warmup_timeout();
        Self {
            writer: parts.writer,
            child: parts.child,
            venv_path,
            session,
            last_used: Instant::now(),
            reader_task,
            next_id: parts.next_id,
            warmup_state: if timeout.is_zero() {
                WarmupState::Ready
            } else {
                WarmupState::Warming
            },
            warmup_deadline: Instant::now() + timeout,
            warmup_queue: Vec::new(),
            venv_token,
            last_venv_check: Instant::now(),
            venv_missing_since: None,
        }
    }

    /// Check if this backend is still warming up
    pub fn is_warming(&self) -> bool {
        self.warmup_state == WarmupState::Warming
    }

    /// Transition from Warming to Ready, returning queued messages for draining
    pub fn mark_ready(&mut self) -> Vec<RpcMessage> {
        self.warmup_state = WarmupState::Ready;
        let queued = std::mem::take(&mut self.warmup_queue);
        if !queued.is_empty() {
            tracing::info!(
                venv = %self.venv_path.display(),
                queued_count = queued.len(),
                "Warmup complete, draining queued requests"
            );
        }
        queued
    }

    /// Check if the warmup deadline has passed
    pub fn warmup_expired(&self) -> bool {
        Instant::now() >= self.warmup_deadline
    }

    /// Queue an index-dependent request during warmup, unless the queue is
    /// already at `MAX_WARMUP_QUEUE_LEN`. Returns `false` if the request was
    /// rejected for capacity, in which case the caller must send an error
    /// response instead of queueing.
    pub fn try_queue_warmup_request(&mut self, msg: RpcMessage) -> bool {
        if self.warmup_queue.len() >= MAX_WARMUP_QUEUE_LEN {
            return false;
        }
        self.warmup_queue.push(msg);
        if self.warmup_queue.len() == WARMUP_QUEUE_WARN_THRESHOLD {
            tracing::warn!(
                venv = %self.venv_path.display(),
                queue_len = self.warmup_queue.len(),
                max = MAX_WARMUP_QUEUE_LEN,
                "Warmup queue approaching capacity"
            );
        }
        true
    }

    /// Remove a queued request by its JSON-RPC id (for $/cancelRequest handling).
    /// Returns the removed message if found.
    pub fn cancel_warmup_request(&mut self, id: &RpcId) -> Option<RpcMessage> {
        if let Some(pos) = self
            .warmup_queue
            .iter()
            .position(|msg| msg.id.as_ref() == Some(id))
        {
            Some(self.warmup_queue.remove(pos))
        } else {
            None
        }
    }
}

/// Pool of backend processes keyed by venv path
pub struct BackendPool {
    backends: HashMap<PathBuf, BackendInstance>,
    pub backend_msg_tx: mpsc::Sender<BackendMessage>,
    pub backend_msg_rx: mpsc::Receiver<BackendMessage>,
    max_backends: usize,
    backend_ttl: Option<Duration>,
    next_session: u64,
}

impl BackendPool {
    pub fn new(max_backends: usize, backend_ttl: Option<Duration>) -> Self {
        let (tx, rx) = mpsc::channel(1024);
        Self {
            backends: HashMap::new(),
            backend_msg_tx: tx,
            backend_msg_rx: rx,
            max_backends,
            backend_ttl,
            next_session: 0,
        }
    }

    /// Get immutable reference to a backend instance
    pub fn get(&self, venv_path: &PathBuf) -> Option<&BackendInstance> {
        self.backends.get(venv_path)
    }

    /// Get mutable reference to a backend instance
    pub fn get_mut(&mut self, venv_path: &PathBuf) -> Option<&mut BackendInstance> {
        self.backends.get_mut(venv_path)
    }

    /// Check if a backend exists for the given venv path
    pub fn contains(&self, venv_path: &PathBuf) -> bool {
        self.backends.contains_key(venv_path)
    }

    /// Insert a backend instance into the pool
    pub fn insert(&mut self, venv_path: PathBuf, instance: BackendInstance) {
        self.backends.insert(venv_path, instance);
    }

    /// Remove a backend instance from the pool
    pub fn remove(&mut self, venv_path: &PathBuf) -> Option<BackendInstance> {
        self.backends.remove(venv_path)
    }

    /// Find the LRU (least recently used) venv path.
    /// Prefers backends with no pending requests (caller provides the count).
    /// Returns None if pool is empty.
    pub fn lru_venv(&self, pending_count_fn: impl Fn(&PathBuf, u64) -> usize) -> Option<PathBuf> {
        // First try: find LRU among backends with 0 pending requests
        let no_pending_lru = self
            .backends
            .iter()
            .filter(|(venv, inst)| pending_count_fn(venv, inst.session) == 0)
            .min_by_key(|(_, inst)| inst.last_used)
            .map(|(venv, _)| venv.clone());

        if no_pending_lru.is_some() {
            return no_pending_lru;
        }

        // Fallback: LRU among all backends
        self.backends
            .iter()
            .min_by_key(|(_, inst)| inst.last_used)
            .map(|(venv, _)| venv.clone())
    }

    /// Generate a new unique session ID
    pub fn next_session_id(&mut self) -> u64 {
        self.next_session += 1;
        self.next_session
    }

    /// Check if pool is at capacity
    pub fn is_full(&self) -> bool {
        self.backends.len() >= self.max_backends
    }

    /// Number of backends in the pool
    pub fn len(&self) -> usize {
        self.backends.len()
    }

    /// Check if pool has no backends
    pub fn is_empty(&self) -> bool {
        self.backends.is_empty()
    }

    /// Get max backends setting
    pub fn max_backends(&self) -> usize {
        self.max_backends
    }

    /// Return venv paths of backends whose `last_used` exceeds the TTL.
    /// Only checks `TTL/last_used`; pending request filtering is the caller's responsibility.
    pub fn expired_venvs(&self) -> Vec<PathBuf> {
        let ttl = match self.backend_ttl {
            Some(ttl) => ttl,
            None => return Vec::new(),
        };

        let now = Instant::now();
        self.backends
            .iter()
            .filter(|(_, inst)| now.duration_since(inst.last_used) >= ttl)
            .map(|(venv, _)| venv.clone())
            .collect()
    }

    /// Get a clone of the sender for spawning reader tasks
    pub fn msg_sender(&self) -> mpsc::Sender<BackendMessage> {
        self.backend_msg_tx.clone()
    }

    /// Get all backend venv keys (for iteration without borrow conflicts)
    pub fn backends_keys(&self) -> Vec<PathBuf> {
        self.backends.keys().cloned().collect()
    }

    /// Get the first key in the map (arbitrary, for fallback routing)
    pub fn first_key(&self) -> Option<&PathBuf> {
        self.backends.keys().next()
    }

    /// Return venv paths of backends currently in Warming state
    pub fn warming_backends(&self) -> Vec<PathBuf> {
        self.backends
            .iter()
            .filter(|(_, inst)| inst.is_warming())
            .map(|(venv, _)| venv.clone())
            .collect()
    }

    /// Return the nearest warmup deadline among all warming backends.
    /// Returns None if no backends are warming.
    pub fn nearest_warmup_deadline(&self) -> Option<Instant> {
        self.backends
            .values()
            .filter(|inst| inst.is_warming())
            .map(|inst| inst.warmup_deadline)
            .min()
    }
}

/// Spawn a reader task that reads messages from a backend and sends them to the channel
pub fn spawn_reader_task(
    mut reader: LspFrameReader<ChildStdout>,
    tx: mpsc::Sender<BackendMessage>,
    venv_path: PathBuf,
    session: u64,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let result = reader
                .read_message()
                .await
                .map_err(BackendError::Communication);

            let is_err = result.is_err();

            let msg = BackendMessage {
                venv_path: venv_path.clone(),
                session,
                result,
            };

            if tx.send(msg).await.is_err() {
                // Channel closed (proxy shutting down)
                tracing::debug!(
                    venv = %venv_path.display(),
                    session = session,
                    "Reader task: channel closed, stopping"
                );
                break;
            }

            if is_err {
                // Backend read error (crash, EOF) — send the error and stop
                tracing::info!(
                    venv = %venv_path.display(),
                    session = session,
                    "Reader task: backend read error, stopping"
                );
                break;
            }
        }
    })
}

/// Shutdown and clean up a backend instance (abort reader, fire-and-forget shutdown)
pub fn shutdown_backend_instance(instance: BackendInstance) {
    instance.reader_task.abort();
    let venv_display = instance.venv_path.display().to_string();
    shutdown_fire_and_forget(
        instance.writer,
        instance.child,
        instance.next_id,
        venv_display,
    );
}
