use crate::backend::PyrightBackend;
use crate::backend_pool::{shutdown_backend_instance, spawn_reader_task, BackendInstance};
use crate::error::ProxyError;
use crate::framing::LspFrameWriter;
use crate::message::RpcMessage;
use std::path::PathBuf;
use tokio::time::Instant;

impl super::LspProxy {
    /// Handle client "initialize" request.
    ///
    /// Caches the message, completes initialization with the pre-spawned
    /// backend (if any), or returns a minimal capabilities response.
    pub(crate) async fn dispatch_initialize(
        &mut self,
        msg: &RpcMessage,
        pending_initial_backend: &mut Option<(PyrightBackend, PathBuf)>,
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
                                let error_message = "pyright-lsp-proxy: .venv not found (strict mode). Create .venv or run hooks.";
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
                                    &format!("pyright-lsp-proxy: backend error: {}", e),
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
                    RpcMessage::error_response(msg, "pyright-lsp-proxy: backend not available");
                client_writer.write_message(&error_response).await?;
            }
        } else {
            // No target backend — check if any backend exists
            if self.state.pool.is_empty() {
                let error_message =
                    "pyright-lsp-proxy: .venv not found (strict mode). Create .venv or run hooks.";
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
}
