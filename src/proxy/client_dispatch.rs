use crate::backend::LspBackend;
use crate::backend_pool::{
    shutdown_backend_instance, spawn_reader_task, warmup_timeout, BackendInstance, WarmupState,
};
use crate::error::ProxyError;
use crate::framing::LspFrameWriter;
use crate::message::{RpcId, RpcMessage};
use std::path::PathBuf;
use tokio::time::Instant;

/// LSP methods that depend on the cross-file index and should be queued during warmup.
const INDEX_DEPENDENT_METHODS: &[&str] = &[
    "textDocument/definition",
    "textDocument/references",
    "textDocument/implementation",
    "textDocument/typeDefinition",
];

impl super::LspProxy {
    /// Handle client "initialize" request.
    ///
    /// Caches the message, completes initialization with the pre-spawned
    /// backend (if any), or returns a minimal capabilities response.
    pub(crate) async fn dispatch_initialize(
        &mut self,
        msg: &RpcMessage,
        pending_initial_backend: &mut Option<(LspBackend, PathBuf)>,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<(), ProxyError> {
        tracing::info!("Caching initialize message for backend initialization");
        self.state.client_initialize = Some(msg.clone());

        if let Some((mut backend, venv)) = pending_initial_backend.take() {
            // Forward initialize to the pre-spawned backend
            match self
                .complete_backend_initialization(&mut backend, &venv, client_writer)
                .await
            {
                Ok(init_response) => {
                    // Split and insert into pool
                    let session = self.state.pool.next_session_id();
                    let parts = backend.into_split();
                    let tx = self.state.pool.msg_sender();
                    let reader_task = spawn_reader_task(parts.reader, tx, venv.clone(), session);

                    let timeout = warmup_timeout();
                    let instance = BackendInstance {
                        writer: parts.writer,
                        child: parts.child,
                        venv_path: venv.clone(),
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

        Ok(())
    }

    /// Handle client "initialized" notification.
    ///
    /// Forwards the notification to all backends in the pool.
    pub(crate) async fn dispatch_initialized(&mut self) -> Result<(), ProxyError> {
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

        Ok(())
    }

    /// Handle client "shutdown" request.
    ///
    /// Shuts down all backends and sends a response to the client.
    pub(crate) async fn dispatch_shutdown(
        &mut self,
        msg: &RpcMessage,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<(), ProxyError> {
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

        Ok(())
    }

    /// Handle a client response (to a server→client request from backend).
    ///
    /// Returns `Ok(true)` if the message was handled (caller should `continue`),
    /// `Ok(false)` if it did not match a pending backend request (fall through).
    pub(crate) async fn dispatch_client_response(
        &mut self,
        msg: &RpcMessage,
    ) -> Result<bool, ProxyError> {
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
                return Ok(true);
            }
            // If not in pending_backend_requests, fall through (shouldn't happen normally)
        }

        Ok(false)
    }

    /// Handle a generic client request (not initialize/shutdown/textDocument notifications).
    ///
    /// Routes the request to the appropriate backend, creating one if necessary.
    pub(crate) async fn dispatch_client_request(
        &mut self,
        msg: &RpcMessage,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<(), ProxyError> {
        const VENV_CHECK_METHODS: &[&str] = &[
            "textDocument/hover",
            "textDocument/definition",
            "textDocument/references",
            "textDocument/documentSymbol",
            "textDocument/typeDefinition",
            "textDocument/implementation",
        ];

        let method = msg.method_name();
        let mut target_venv: Option<PathBuf> = None;

        // For VENV_CHECK_METHODS, ensure the correct backend is in the pool
        if let Some(method_name) = method {
            if VENV_CHECK_METHODS.contains(&method_name) {
                if let Some(url) = Self::extract_text_document_uri(msg) {
                    if let Ok(file_path) = url.to_file_path() {
                        match self
                            .ensure_backend_in_pool(&url, &file_path, client_writer)
                            .await
                        {
                            Ok(Some(venv)) => {
                                target_venv = Some(venv);
                            }
                            Ok(None) => {
                                // No venv found — return error
                                let error_message = "lsp-proxy: .venv not found (strict mode). Create .venv or run hooks.";
                                tracing::warn!(
                                    method = method_name,
                                    uri = %url,
                                    "No venv found, returning error"
                                );
                                let error_response = RpcMessage::error_response(msg, error_message);
                                client_writer.write_message(&error_response).await?;
                                return Ok(());
                            }
                            Err(e) => {
                                tracing::error!(error = ?e, "Failed to ensure backend in pool");
                                let error_response = RpcMessage::error_response(
                                    msg,
                                    &format!("lsp-proxy: backend error: {}", e),
                                );
                                client_writer.write_message(&error_response).await?;
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }

        // Determine target backend if not yet determined
        if target_venv.is_none() {
            if let Some(url) = Self::extract_text_document_uri(msg) {
                target_venv = self.venv_for_uri(&url);
            }
        }

        // If we have a target, send to that backend
        if let Some(ref venv_path) = target_venv {
            if let Some(inst) = self.state.pool.get_mut(venv_path) {
                inst.last_used = Instant::now();
                let session = inst.session;

                // Queue index-dependent requests during warmup
                if let Some(method_name) = method {
                    if inst.is_warming() && INDEX_DEPENDENT_METHODS.contains(&method_name) {
                        // Register in pending requests (so cancel/crash handling works)
                        if let Some(id) = &msg.id {
                            self.state.pending_requests.insert(
                                id.clone(),
                                crate::state::PendingRequest {
                                    backend_session: session,
                                    venv_path: venv_path.clone(),
                                },
                            );
                        }
                        tracing::info!(
                            method = method_name,
                            id = ?msg.id,
                            venv = %venv_path.display(),
                            "Queueing index-dependent request during warmup"
                        );
                        inst.warmup_queue.push(msg.clone());
                        return Ok(());
                    }
                }

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

                if let Err(e) = inst.writer.write_message(msg).await {
                    tracing::error!(venv = %venv_path.display(), error = ?e, "Failed to send request to backend");
                }
            } else {
                // Backend disappeared (race with crash handling)
                let error_response =
                    RpcMessage::error_response(msg, "lsp-proxy: backend not available");
                client_writer.write_message(&error_response).await?;
            }
        } else {
            // No target backend — check if any backend exists
            if self.state.pool.is_empty() {
                let error_message =
                    "lsp-proxy: .venv not found (strict mode). Create .venv or run hooks.";
                let error_response = RpcMessage::error_response(msg, error_message);
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
                        if let Err(e) = inst.writer.write_message(msg).await {
                            tracing::error!(venv = %venv_path.display(), error = ?e, "Failed to send request to backend");
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Handle a generic client notification (not handled by specific handlers above).
    ///
    /// Forwards to all backends in the pool.
    pub(crate) async fn dispatch_client_notification(
        &mut self,
        msg: &RpcMessage,
    ) -> Result<(), ProxyError> {
        let venvs: Vec<PathBuf> = self.state.pool.backends_keys();
        for venv in &venvs {
            if let Some(inst) = self.state.pool.get_mut(venv) {
                if let Err(e) = inst.writer.write_message(msg).await {
                    tracing::warn!(venv = %venv.display(), error = ?e, "Failed to forward notification to backend");
                }
            }
        }

        Ok(())
    }

    /// Handle `$/cancelRequest` notification.
    ///
    /// If the target request is queued in a warmup queue, remove it without
    /// forwarding to the backend. Otherwise, forward the cancel to all backends.
    pub(crate) async fn dispatch_cancel_request(
        &mut self,
        msg: &RpcMessage,
    ) -> Result<(), ProxyError> {
        if let Some(cancelled_id) = extract_cancel_id(msg) {
            if let Some(pending) = self.state.pending_requests.get(&cancelled_id).cloned() {
                if let Some(inst) = self.state.pool.get_mut(&pending.venv_path) {
                    if inst.session == pending.backend_session
                        && inst.cancel_warmup_request(&cancelled_id).is_some()
                    {
                        tracing::info!(
                            id = ?cancelled_id,
                            venv = %pending.venv_path.display(),
                            "Cancelled warmup-queued request"
                        );
                        self.state.pending_requests.remove(&cancelled_id);
                        return Ok(());
                    }
                }
            }
        }

        // Not in warmup queue — forward $/cancelRequest to all backends
        self.dispatch_client_notification(msg).await
    }

    /// Forward queued warmup requests to the backend now that it is ready.
    /// `expected_session` is checked to avoid forwarding to a replaced backend.
    pub(crate) async fn drain_warmup_queue(
        &mut self,
        venv_path: &PathBuf,
        expected_session: u64,
        queued: Vec<RpcMessage>,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<(), ProxyError> {
        for request in queued {
            let method = request.method_name().unwrap_or("unknown").to_string();
            let id_debug = format!("{:?}", request.id);

            // Session guard: if the backend was replaced (crash + re-create),
            // discard remaining queued requests instead of forwarding to the new session.
            let session_ok = self
                .state
                .pool
                .get(venv_path)
                .is_some_and(|inst| inst.session == expected_session);
            if !session_ok {
                tracing::warn!(
                    method = %method,
                    id = %id_debug,
                    venv = %venv_path.display(),
                    "Aborting warmup drain: backend session changed"
                );
                // Remove remaining queued requests from pending_requests
                if let Some(req_id) = &request.id {
                    self.state.pending_requests.remove(req_id);
                }
                continue;
            }

            if let Some(inst) = self.state.pool.get_mut(venv_path) {
                match inst.writer.write_message(&request).await {
                    Ok(()) => {
                        tracing::info!(
                            method = %method,
                            id = %id_debug,
                            venv = %venv_path.display(),
                            "Draining warmup queue: forwarding request"
                        );
                    }
                    Err(e) => {
                        tracing::error!(
                            method = %method,
                            id = %id_debug,
                            venv = %venv_path.display(),
                            error = ?e,
                            "Failed to forward warmup-queued request"
                        );
                        // Remove from pending_requests and send error to client
                        if let Some(req_id) = &request.id {
                            self.state.pending_requests.remove(req_id);
                        }
                        let error_response = RpcMessage::error_response(
                            &request,
                            "lsp-proxy: backend write failed during warmup drain",
                        );
                        client_writer.write_message(&error_response).await?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// Extract the cancel target id from a `$/cancelRequest` params.
fn extract_cancel_id(msg: &RpcMessage) -> Option<RpcId> {
    let params = msg.params.as_ref()?;
    let id_value = params.get("id")?;
    if let Some(n) = id_value.as_i64() {
        Some(RpcId::Number(n))
    } else {
        id_value.as_str().map(|s| RpcId::String(s.to_string()))
    }
}
