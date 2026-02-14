mod backend_dispatch;
mod client_dispatch;
mod diagnostics;
mod document;
mod initialization;
mod pool_management;

use crate::backend::{BackendKind, LspBackend};
use crate::error::ProxyError;
use crate::framing::{LspFrameReader, LspFrameWriter};
use crate::state::ProxyState;
use crate::venv;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{stdin, stdout};
use tokio::time::{Instant, MissedTickBehavior};

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
            let backend = LspBackend::spawn(self.state.backend_kind, Some(&venv)).await?;
            Some((backend, venv))
        } else {
            tracing::warn!("No fallback .venv found, starting with empty pool");
            None
        };

        let mut didopen_count = 0;

        // TTL sweep timer: checks every 60 seconds for expired backends
        let mut ttl_interval = tokio::time::interval(std::time::Duration::from_secs(60));
        ttl_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // Consume the first immediate tick so the first real tick fires after 60s
        ttl_interval.tick().await;

        loop {
            tokio::select! {
                // Messages from client
                result = client_reader.read_message() => {
                    let msg = result?;
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
                            self.dispatch_initialize(&msg, &mut pending_initial_backend, &mut client_writer).await?;
                        }
                        Some("initialized") => {
                            self.dispatch_initialized().await?;
                        }
                        Some("shutdown") => {
                            self.dispatch_shutdown(&msg, &mut client_writer).await?;
                        }
                        Some("exit") => {
                            tracing::info!("Received exit notification, terminating proxy");
                            return Ok(());
                        }
                        _ if msg.is_response() => {
                            if self.dispatch_client_response(&msg).await? {
                                continue;
                            }
                            // Fall through: not a pending backend request
                            // (original code fell through to didOpen check etc.)
                        }
                        Some("textDocument/didOpen") => {
                            didopen_count += 1;
                            self.handle_did_open(&msg, didopen_count, &mut client_writer).await?;
                        }
                        Some("textDocument/didChange") => {
                            self.handle_did_change(&msg).await?;
                            // Forward to appropriate backend
                            if let Some(url) = Self::extract_text_document_uri(&msg) {
                                if let Some(venv_path) = self.venv_for_uri(&url) {
                                    if let Some(inst) = self.state.pool.get_mut(&venv_path) {
                                        inst.last_used = Instant::now();
                                        if let Err(e) = inst.writer.write_message(&msg).await {
                                            tracing::warn!(venv = %venv_path.display(), error = ?e, "Failed to forward didChange");
                                        }
                                    }
                                }
                            }
                        }
                        Some("textDocument/didClose") => {
                            // Get venv before removing from cache
                            let venv_for_close = Self::extract_text_document_uri(&msg)
                                .and_then(|url| self.venv_for_uri(&url));

                            self.handle_did_close(&msg).await?;

                            // Forward to appropriate backend
                            if let Some(venv_path) = venv_for_close {
                                if let Some(inst) = self.state.pool.get_mut(&venv_path) {
                                    inst.last_used = Instant::now();
                                    if let Err(e) = inst.writer.write_message(&msg).await {
                                        tracing::warn!(venv = %venv_path.display(), error = ?e, "Failed to forward didClose");
                                    }
                                }
                            }
                        }
                        _ if msg.is_request() => {
                            self.dispatch_client_request(&msg, &mut client_writer).await?;
                        }
                        _ if msg.is_notification() => {
                            self.dispatch_client_notification(&msg).await?;
                        }
                        _ => {}
                    }
                }

                // Messages from all backends via mpsc channel
                Some(backend_msg) = self.state.pool.backend_msg_rx.recv() => {
                    self.dispatch_backend_message(backend_msg, &mut client_writer).await?;
                }

                // TTL-based auto-eviction sweep
                _ = ttl_interval.tick(), if self.backend_ttl.is_some() => {
                    self.evict_expired_backends(&mut client_writer).await?;
                }
            }
        }
    }
}
