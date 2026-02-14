use crate::backend::shutdown_fire_and_forget;
use crate::error::BackendError;
use crate::framing::{LspFrameReader, LspFrameWriter};
use crate::message::{RpcId, RpcMessage};
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

/// Default warmup timeout; overridable via `TYPEMUX_CC_WARMUP_TIMEOUT` env var.
const DEFAULT_WARMUP_TIMEOUT: Duration = Duration::from_secs(2);

/// Returns the warmup timeout duration.
/// `TYPEMUX_CC_WARMUP_TIMEOUT=0` means warmup is disabled (immediate Ready).
pub fn warmup_timeout() -> Duration {
    std::env::var("TYPEMUX_CC_WARMUP_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_WARMUP_TIMEOUT)
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
}

impl BackendInstance {
    /// Get next request ID for this backend (used for shutdown messages)
    #[allow(dead_code)]
    pub fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
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

    /// Return venv paths of backends whose last_used exceeds the TTL.
    /// Only checks TTL/last_used; pending request filtering is the caller's responsibility.
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
                .map_err(|e| BackendError::SpawnFailed(std::io::Error::other(e)));

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
