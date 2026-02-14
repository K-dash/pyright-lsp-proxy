mod backend_dispatch;
mod diagnostics;
mod document;
mod initialization;
mod pool_management;

use crate::backend::PyrightBackend;
use crate::backend_pool::{shutdown_backend_instance, spawn_reader_task, BackendInstance};
use crate::error::ProxyError;
use crate::framing::{LspFrameReader, LspFrameWriter};
use crate::message::RpcMessage;
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
    pub fn new(max_backends: usize, backend_ttl: Option<Duration>) -> Self {
        Self {
            state: ProxyState::new(max_backends, backend_ttl),
            backend_ttl,
        }
    }

    pub async fn run(&mut self) -> Result<(), ProxyError> {
        let mut client_reader = LspFrameReader::new(stdin());
        let mut client_writer = LspFrameWriter::new(stdout());

        let cwd = std::env::current_dir()?;
        tracing::info!(
            cwd = %cwd.display(),
            max_backends = self.state.pool.max_backends(),
            backend_ttl = ?self.backend_ttl.map(|d| format!("{}s", d.as_secs())),
            "Starting pyright-lsp-proxy"
        );

        // Get and cache git toplevel
        self.state.git_toplevel = venv::get_git_toplevel(&cwd).await?;

        // Search for fallback venv
        let fallback_venv = venv::find_fallback_venv(&cwd).await?;

        // Pre-spawn backend if fallback venv found (but don't insert into pool yet —
        // wait for client's `initialize` to complete the handshake first)
        let mut pending_initial_backend: Option<(PyrightBackend, PathBuf)> = if let Some(venv) =
            fallback_venv
        {
            tracing::info!(venv = %venv.display(), "Using fallback .venv, pre-spawning backend");
            let backend = PyrightBackend::spawn(Some(&venv)).await?;
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

                    // Handle initialize
                    if method == Some("initialize") {
                        tracing::info!("Caching initialize message for backend initialization");
                        self.state.client_initialize = Some(msg.clone());

                        if let Some((mut backend, venv)) = pending_initial_backend.take() {
                            // Forward initialize to the pre-spawned backend
                            match self.complete_backend_initialization(&mut backend, &venv, &mut client_writer).await {
                                Ok(init_response) => {
                                    // Split and insert into pool
                                    let session = self.state.pool.next_session_id();
                                    let parts = backend.into_split();
                                    let tx = self.state.pool.msg_sender();
                                    let reader_task = spawn_reader_task(parts.reader, tx, venv.clone(), session);

                                    let instance = BackendInstance {
                                        writer: parts.writer,
                                        child: parts.child,
                                        venv_path: venv.clone(),
                                        session,
                                        last_used: Instant::now(),
                                        reader_task,
                                        next_id: parts.next_id,
                                    };
                                    self.state.pool.insert(venv, instance);

                                    // Send initialize response to client
                                    client_writer.write_message(&init_response).await?;
                                    tracing::info!("Initial backend inserted into pool");
                                }
                                Err(e) => {
                                    tracing::error!(error = ?e, "Failed to initialize fallback backend, returning minimal response");
                                    let init_response = RpcMessage {
                                        jsonrpc: "2.0".to_string(),
                                        id: msg.id.clone(),
                                        method: None,
                                        params: None,
                                        result: Some(serde_json::json!({
                                            "capabilities": {}
                                        })),
                                        error: None,
                                    };
                                    client_writer.write_message(&init_response).await?;
                                }
                            }
                        } else {
                            // No fallback backend — return minimal capabilities
                            tracing::warn!("No fallback backend: returning minimal initialize response");
                            let init_response = RpcMessage {
                                jsonrpc: "2.0".to_string(),
                                id: msg.id.clone(),
                                method: None,
                                params: None,
                                result: Some(serde_json::json!({
                                    "capabilities": {}
                                })),
                                error: None,
                            };
                            client_writer.write_message(&init_response).await?;
                        }
                        continue;
                    }

                    // Handle initialized notification
                    if method == Some("initialized") {
                        tracing::info!("Client initialized");
                        // Forward to all backends in the pool
                        let initialized_msg = RpcMessage {
                            jsonrpc: "2.0".to_string(),
                            id: None,
                            method: Some("initialized".to_string()),
                            params: Some(serde_json::json!({})),
                            result: None,
                            error: None,
                        };
                        // Collect keys to avoid borrow issues
                        let venvs: Vec<PathBuf> = self.state.pool.backends_keys();
                        for venv in &venvs {
                            if let Some(inst) = self.state.pool.get_mut(venv) {
                                if let Err(e) = inst.writer.write_message(&initialized_msg).await {
                                    tracing::warn!(venv = %venv.display(), error = ?e, "Failed to forward initialized to backend");
                                }
                            }
                        }
                        continue;
                    }

                    // Handle shutdown request
                    if method == Some("shutdown") {
                        tracing::info!("Received shutdown request from client");

                        // Shutdown all backends in the pool
                        let venvs: Vec<PathBuf> = self.state.pool.backends_keys();
                        for venv in &venvs {
                            if let Some(instance) = self.state.pool.remove(venv) {
                                tracing::info!(venv = %venv.display(), "Shutting down backend");
                                shutdown_backend_instance(instance);
                            }
                        }

                        // Send shutdown response to client
                        let shutdown_response = RpcMessage {
                            jsonrpc: "2.0".to_string(),
                            id: msg.id.clone(),
                            method: None,
                            params: None,
                            result: Some(serde_json::Value::Null),
                            error: None,
                        };
                        client_writer.write_message(&shutdown_response).await?;
                        tracing::info!("Sent shutdown response to client");
                        continue;
                    }

                    // Handle exit notification
                    if method == Some("exit") {
                        tracing::info!("Received exit notification, terminating proxy");
                        return Ok(());
                    }

                    // Handle client response (to a server→client request from backend)
                    if msg.is_response() {
                        if let Some(proxy_id) = &msg.id {
                            if let Some(pending) = self.state.pending_backend_requests.remove(proxy_id) {
                                // Restore original backend ID and route to correct backend
                                let mut response_msg = msg.clone();
                                response_msg.id = Some(pending.original_id);

                                if let Some(inst) = self.state.pool.get_mut(&pending.venv_path) {
                                    if inst.session == pending.session {
                                        if let Err(e) = inst.writer.write_message(&response_msg).await {
                                            tracing::warn!(
                                                venv = %pending.venv_path.display(),
                                                error = ?e,
                                                "Failed to forward client response to backend"
                                            );
                                        }
                                    } else {
                                        tracing::warn!(
                                            proxy_id = ?proxy_id,
                                            expected_session = pending.session,
                                            actual_session = inst.session,
                                            "Discarding client response: session mismatch"
                                        );
                                    }
                                } else {
                                    tracing::warn!(
                                        proxy_id = ?proxy_id,
                                        venv = %pending.venv_path.display(),
                                        "Discarding client response: backend no longer in pool"
                                    );
                                }
                                continue;
                            }
                            // If not in pending_backend_requests, fall through (shouldn't happen normally)
                        }
                    }

                    // Handle didOpen
                    if method == Some("textDocument/didOpen") {
                        didopen_count += 1;
                        self.handle_did_open(&msg, didopen_count, &mut client_writer).await?;
                        continue;
                    }

                    // Handle didChange (always update cache)
                    if method == Some("textDocument/didChange") {
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
                        continue;
                    }

                    // Handle didClose (always update cache)
                    if method == Some("textDocument/didClose") {
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
                        continue;
                    }

                    // Request processing
                    const VENV_CHECK_METHODS: &[&str] = &[
                        "textDocument/hover",
                        "textDocument/definition",
                        "textDocument/references",
                        "textDocument/documentSymbol",
                        "textDocument/typeDefinition",
                        "textDocument/implementation",
                    ];

                    if msg.is_request() {
                        let m = method;
                        let mut target_venv: Option<PathBuf> = None;

                        // For VENV_CHECK_METHODS, ensure the correct backend is in the pool
                        if let Some(method_name) = m {
                            if VENV_CHECK_METHODS.contains(&method_name) {
                                if let Some(url) = Self::extract_text_document_uri(&msg) {
                                    if let Ok(file_path) = url.to_file_path() {
                                        match self.ensure_backend_in_pool(&url, &file_path, &mut client_writer).await {
                                            Ok(Some(venv)) => {
                                                target_venv = Some(venv);
                                            }
                                            Ok(None) => {
                                                // No venv found — return error
                                                let error_message = "pyright-lsp-proxy: .venv not found (strict mode). Create .venv or run hooks.";
                                                tracing::warn!(
                                                    method = method_name,
                                                    uri = %url,
                                                    "No venv found, returning error"
                                                );
                                                let error_response = RpcMessage::error_response(&msg, error_message);
                                                client_writer.write_message(&error_response).await?;
                                                continue;
                                            }
                                            Err(e) => {
                                                tracing::error!(error = ?e, "Failed to ensure backend in pool");
                                                let error_response = RpcMessage::error_response(&msg, &format!("pyright-lsp-proxy: backend error: {}", e));
                                                client_writer.write_message(&error_response).await?;
                                                continue;
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        // Determine target backend if not yet determined
                        if target_venv.is_none() {
                            if let Some(url) = Self::extract_text_document_uri(&msg) {
                                target_venv = self.venv_for_uri(&url);
                            }
                        }

                        // If we have a target, send to that backend
                        if let Some(ref venv_path) = target_venv {
                            if let Some(inst) = self.state.pool.get_mut(venv_path) {
                                inst.last_used = Instant::now();
                                let session = inst.session;

                                // Register in pending requests
                                if let Some(id) = &msg.id {
                                    self.state.pending_requests.insert(
                                        id.clone(),
                                        crate::state::PendingRequest {
                                            backend_session: session,
                                            venv_path: venv_path.clone(),
                                        },
                                    );
                                }

                                if let Err(e) = inst.writer.write_message(&msg).await {
                                    tracing::error!(venv = %venv_path.display(), error = ?e, "Failed to send request to backend");
                                }
                            } else {
                                // Backend disappeared (race with crash handling)
                                let error_response = RpcMessage::error_response(&msg, "pyright-lsp-proxy: backend not available");
                                client_writer.write_message(&error_response).await?;
                            }
                        } else {
                            // No target backend — check if any backend exists
                            if self.state.pool.is_empty() {
                                let error_message = "pyright-lsp-proxy: .venv not found (strict mode). Create .venv or run hooks.";
                                let error_response = RpcMessage::error_response(&msg, error_message);
                                client_writer.write_message(&error_response).await?;
                            } else {
                                // Forward to the first available backend (best effort for non-textDocument requests)
                                let first_venv = self.state.pool.first_key().cloned();
                                if let Some(venv_path) = first_venv {
                                    if let Some(inst) = self.state.pool.get_mut(&venv_path) {
                                        inst.last_used = Instant::now();
                                        let session = inst.session;
                                        if let Some(id) = &msg.id {
                                            self.state.pending_requests.insert(
                                                id.clone(),
                                                crate::state::PendingRequest {
                                                    backend_session: session,
                                                    venv_path: venv_path.clone(),
                                                },
                                            );
                                        }
                                        if let Err(e) = inst.writer.write_message(&msg).await {
                                            tracing::error!(venv = %venv_path.display(), error = ?e, "Failed to send request to backend");
                                        }
                                    }
                                }
                            }
                        }
                        continue;
                    }

                    // Non-request, non-notification that's not handled above — forward to all backends
                    // (This shouldn't normally happen, but be defensive)
                    if msg.is_notification() {
                        let venvs: Vec<PathBuf> = self.state.pool.backends_keys();
                        for venv in &venvs {
                            if let Some(inst) = self.state.pool.get_mut(venv) {
                                if let Err(e) = inst.writer.write_message(&msg).await {
                                    tracing::warn!(venv = %venv.display(), error = ?e, "Failed to forward notification to backend");
                                }
                            }
                        }
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
