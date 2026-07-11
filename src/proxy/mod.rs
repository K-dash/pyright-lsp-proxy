mod backend_dispatch;
mod client_dispatch;
mod diagnostics;
mod document;
mod fanout;
mod initialization;
mod pool_management;

use crate::backend::{BackendKind, LspBackend};
use crate::error::ProxyError;
use crate::framing::{LspFrameReader, LspFrameWriter};
use crate::message::RpcMessage;
use crate::state::ProxyState;
use crate::venv;
use pool_management::StaleOutcome;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{stdin, stdout};
use tokio::time::MissedTickBehavior;

pub struct LspProxy {
    state: ProxyState,
    backend_ttl: Option<Duration>,
}

impl LspProxy {
    pub fn new(
        backend_kind: BackendKind,
        max_backends: usize,
        backend_ttl: Option<Duration>,
    ) -> Self {
        Self {
            state: ProxyState::new(backend_kind, max_backends, backend_ttl),
            backend_ttl,
        }
    }

    /// Dispatch one client-originated message: either read directly from the
    /// client, or replayed from `ProxyState::replay_queue` after arriving
    /// while its venv was `Creating` (see the `creation_rx`/replay select!
    /// arms in `run`). Replayed messages are modeled as ordinary client
    /// messages that arrived early and go through this exact same dispatch —
    /// they can never be `initialize`/`initialized`/`shutdown`/`exit`
    /// (nothing queues those into a Creating entry).
    ///
    /// Returns `Ok(true)` if the proxy should terminate (client sent `exit`).
    async fn dispatch_client_message(
        &mut self,
        msg: RpcMessage,
        pending_initial_backend: &mut Option<(LspBackend, PathBuf)>,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
        didopen_count: &mut usize,
    ) -> Result<bool, ProxyError> {
        let method = msg.method_name();

        tracing::debug!(
            method = ?method,
            is_request = msg.is_request(),
            is_notification = msg.is_notification(),
            "Client -> Proxy"
        );

        // Dispatch based on method, preserving original if-chain order
        match method {
            Some("initialize") => {
                self.dispatch_initialize(&msg, pending_initial_backend, client_writer)
                    .await?;
            }
            Some("initialized") => {
                self.dispatch_initialized().await?;
            }
            Some("shutdown") => {
                self.dispatch_shutdown(&msg, client_writer).await?;
            }
            Some("exit") => {
                tracing::info!("Received exit notification, terminating proxy");
                return Ok(true);
            }
            _ if msg.is_response() && self.dispatch_client_response(&msg).await? => {}
            Some("textDocument/didOpen") => {
                *didopen_count += 1;
                self.handle_did_open(&msg, *didopen_count, client_writer)
                    .await?;
            }
            Some("textDocument/didChange") => {
                self.handle_did_change(&msg)?;
                // Forward to appropriate backend, after verifying its venv
                // identity hasn't gone stale.
                if let Some(url) = Self::extract_text_document_uri(&msg) {
                    if let Some(venv_path) = self.venv_for_uri(&url) {
                        match self
                            .check_pooled_venv_staleness(&venv_path, client_writer)
                            .await?
                        {
                            StaleOutcome::Fresh => {
                                // Ready → forward; Creating → queue (the
                                // silent-drop bug `forward_to_backend` alone
                                // would hit here, since a Creating venv
                                // isn't in `backends` yet); absent → no-op.
                                self.forward_or_queue_for_venv(&venv_path, &msg, client_writer)
                                    .await?;
                            }
                            // Respawned: creation just started for this venv;
                            // handle_did_change already updated the cache
                            // above, so the fresh text is in the restoration
                            // snapshot the new creation task took. Removed:
                            // revival-preparation mode, nothing to forward to.
                            StaleOutcome::Respawned | StaleOutcome::Removed => {}
                            // Respawn failure is contained: notify the
                            // client; the next request retries lazily.
                            StaleOutcome::RespawnFailed(e) => {
                                self.notify_backend_error(&venv_path, &e, client_writer)
                                    .await;
                            }
                        }
                    }
                }
            }
            Some("textDocument/didClose") => {
                // Get venv before removing from cache
                let venv_for_close =
                    Self::extract_text_document_uri(&msg).and_then(|url| self.venv_for_uri(&url));

                self.handle_did_close(&msg);

                // Forward to appropriate backend. A Creating venv's didClose
                // stays a no-op here (accepted v1 gap — see ARCHITECTURE.md):
                // `forward_to_backend` only writes to a Ready instance.
                if let Some(venv_path) = venv_for_close {
                    self.forward_to_backend(&venv_path, &msg).await?;
                }
            }
            Some("$/cancelRequest") => {
                self.dispatch_cancel_request(&msg, client_writer).await?;
            }
            _ if msg.is_request() => {
                self.dispatch_client_request(&msg, client_writer).await?;
            }
            _ if msg.is_notification() => {
                self.dispatch_client_notification(&msg, client_writer)
                    .await?;
            }
            _ => {}
        }

        Ok(false)
    }

    pub async fn run(&mut self) -> Result<(), ProxyError> {
        let mut client_reader = LspFrameReader::new(stdin());
        let mut client_writer = LspFrameWriter::new(stdout());

        let cwd = std::env::current_dir()?;
        tracing::info!(
            cwd = %cwd.display(),
            backend = self.state.backend_kind.display_name(),
            max_backends = self.state.pool.max_backends(),
            backend_ttl = ?self.backend_ttl.map(|d| format!("{}s", d.as_secs())),
            "Starting LSP proxy"
        );

        // Get and cache git toplevel
        self.state.git_toplevel = venv::get_git_toplevel(&cwd).await?;

        // Search for fallback venv
        let fallback_venv = venv::find_fallback_venv(&cwd).await?;

        // Pre-spawn backend if fallback venv found (but don't insert into pool yet —
        // wait for client's `initialize` to complete the handshake first)
        let mut pending_initial_backend: Option<(LspBackend, PathBuf)> = if let Some(venv) =
            fallback_venv
        {
            tracing::info!(venv = %venv.display(), "Using fallback .venv, pre-spawning backend");
            let backend = LspBackend::spawn(self.state.backend_kind, Some(&venv))?;
            Some((backend, venv))
        } else {
            tracing::warn!("No fallback .venv found, starting with empty pool");
            None
        };

        let mut didopen_count: usize = 0;

        // TTL sweep timer: checks every 60 seconds for expired backends
        let mut ttl_interval = tokio::time::interval(std::time::Duration::from_secs(60));
        ttl_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // Consume the first immediate tick so the first real tick fires after 60s
        ttl_interval.tick().await;

        loop {
            // Compute deadlines before entering select! to avoid borrow conflicts
            let warmup_deadline = self.state.pool.nearest_warmup_deadline();
            let fanout_deadline = self.state.nearest_fanout_deadline();

            tokio::select! {
                // `biased;`: poll arms top-to-bottom, stopping at the first
                // ready one, instead of randomized order. Load-bearing for
                // the replay arm at the bottom: backend_msg_rx must always
                // be drained ahead of replaying a queued message, so a
                // replayed didOpen's diagnostics burst can never starve the
                // channel that's supposed to be draining it (see
                // ARCHITECTURE.md's Creating-state section). All other arms
                // keep their pre-existing relative order.
                biased;

                // Messages from client
                result = client_reader.read_message() => {
                    let msg = result?;
                    if self.dispatch_client_message(msg, &mut pending_initial_backend, &mut client_writer, &mut didopen_count).await? {
                        return Ok(());
                    }
                }

                // Messages from all backends via mpsc channel
                Some(backend_msg) = self.state.pool.backend_msg_rx.recv() => {
                    self.dispatch_backend_message(backend_msg, &mut client_writer).await?;
                }

                // Backend-creation task completion (success or failure).
                // Fast bookkeeping only: insert into the pool / move queued
                // messages into `replay_queue` on success, or send bounded
                // error responses + a dedup'd notification on failure. Never
                // does backend I/O for the newly-created instance itself.
                Some(outcome) = self.state.pool.creation_rx.recv() => {
                    self.handle_creation_outcome(outcome, &mut client_writer).await?;
                }

                // TTL-based auto-eviction and venv-identity staleness sweep.
                // Always armed (not gated on backend_ttl) so the staleness
                // sweep runs even when TTL eviction is disabled.
                _ = ttl_interval.tick() => {
                    if self.backend_ttl.is_some() {
                        self.evict_expired_backends(&mut client_writer).await?;
                    }
                    if !crate::backend_pool::venv_check_interval().is_zero() {
                        self.sweep_stale_venvs(&mut client_writer).await?;
                    }
                }

                // Warmup timeout: fail-open transition for warming backends
                () = async {
                    match warmup_deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    self.expire_warmup_backends(&mut client_writer).await?;
                }

                // Fan-out timeout: return partial results for timed-out fan-out requests
                () = async {
                    match fanout_deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    self.expire_fanout_requests(&mut client_writer).await?;
                }

                // Replay queue: re-dispatch exactly ONE message queued while
                // its venv was Creating, per loop iteration — never a batch
                // loop (a burst of replayed didOpens could otherwise exceed
                // backend_msg_rx's capacity and reintroduce the #93
                // deadlock). Placed last and after `biased;` establishes
                // backend_msg_rx's priority above it, so real backend
                // traffic always drains first.
                Some(replay_msg) = async { self.state.replay_queue.pop_front() }, if !self.state.replay_queue.is_empty() => {
                    if self.dispatch_client_message(replay_msg, &mut pending_initial_backend, &mut client_writer, &mut didopen_count).await? {
                        return Ok(());
                    }
                }
            }
        }
    }
}
