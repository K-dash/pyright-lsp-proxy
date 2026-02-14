mod initialization;
mod pool_management;

use crate::backend::PyrightBackend;
use crate::backend_pool::{
    shutdown_backend_instance, spawn_reader_task, BackendInstance, BackendMessage,
};
use crate::error::ProxyError;
use crate::framing::{LspFrameReader, LspFrameWriter};
use crate::message::RpcMessage;
use crate::state::ProxyState;
use crate::venv;
use std::path::{Path, PathBuf};
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
                    let BackendMessage { venv_path, session, result } = backend_msg;

                    // Stale session check: discard messages from backends no longer in the pool
                    // or whose session has changed (evicted and re-created)
                    let is_current = self.state.pool.get(&venv_path)
                        .is_some_and(|inst| inst.session == session);

                    if !is_current {
                        match result {
                            Ok(_) => {
                                tracing::debug!(
                                    venv = %venv_path.display(),
                                    session = session,
                                    "Discarding stale message from evicted/crashed backend"
                                );
                            }
                            Err(_) => {
                                tracing::debug!(
                                    venv = %venv_path.display(),
                                    session = session,
                                    "Discarding stale error from evicted/crashed backend"
                                );
                            }
                        }
                        continue;
                    }

                    match result {
                        Ok(msg) => {
                            tracing::debug!(
                                venv = %venv_path.display(),
                                session = session,
                                is_response = msg.is_response(),
                                is_notification = msg.is_notification(),
                                is_request = msg.is_request(),
                                "Backend -> Proxy"
                            );

                            // Check if this is a server→client request from the backend
                            if msg.is_request() {
                                if let Some(original_id) = &msg.id {
                                    // Assign a proxy-unique ID to avoid collisions between backends
                                    let proxy_id = self.state.alloc_proxy_request_id();

                                    let pending = crate::state::PendingBackendRequest {
                                        original_id: original_id.clone(),
                                        venv_path: venv_path.clone(),
                                        session,
                                    };
                                    self.state.pending_backend_requests.insert(proxy_id.clone(), pending);

                                    // Rewrite the ID before forwarding to client
                                    let mut forwarded_msg = msg;
                                    forwarded_msg.id = Some(proxy_id);
                                    client_writer.write_message(&forwarded_msg).await?;
                                } else {
                                    // Request without ID (shouldn't happen per JSON-RPC, but be defensive)
                                    client_writer.write_message(&msg).await?;
                                }
                                continue;
                            }

                            // Handle response: check pending + stale check
                            if msg.is_response() {
                                if let Some(id) = &msg.id {
                                    if let Some(pending) = self.state.pending_requests.get(id) {
                                        if pending.backend_session != session || pending.venv_path != venv_path {
                                            tracing::warn!(
                                                id = ?id,
                                                pending_session = pending.backend_session,
                                                pending_venv = %pending.venv_path.display(),
                                                msg_session = session,
                                                msg_venv = %venv_path.display(),
                                                "Discarding stale response from old backend session"
                                            );
                                            self.state.pending_requests.remove(id);
                                            continue;
                                        }
                                    }
                                    self.state.pending_requests.remove(id);
                                }
                            }

                            // Forward to client
                            client_writer.write_message(&msg).await?;
                        }
                        Err(e) => {
                            tracing::error!(
                                venv = %venv_path.display(),
                                session = session,
                                error = ?e,
                                "Backend read error (crash/EOF)"
                            );
                            self.handle_backend_crash(&venv_path, session, &mut client_writer).await?;
                        }
                    }
                }

                // TTL-based auto-eviction sweep
                _ = ttl_interval.tick(), if self.backend_ttl.is_some() => {
                    self.evict_expired_backends(&mut client_writer).await?;
                }
            }
        }
    }

    /// Clear diagnostics for all documents belonging to a venv
    async fn clear_diagnostics_for_venv(
        &self,
        venv_path: &Path,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) {
        let uris_to_clear: Vec<url::Url> = self
            .state
            .open_documents
            .iter()
            .filter(|(_, doc)| doc.venv.as_deref() == Some(venv_path))
            .map(|(url, _)| url.clone())
            .collect();

        let (ok, failed) = self
            .clear_diagnostics_for_uris(&uris_to_clear, client_writer)
            .await;

        if !uris_to_clear.is_empty() {
            tracing::info!(
                venv = %venv_path.display(),
                cleared_ok = ok,
                cleared_failed = failed,
                "Diagnostics cleared for evicted venv"
            );
        }
    }

    /// Extract textDocument.uri from LSP request params
    fn extract_text_document_uri(msg: &RpcMessage) -> Option<url::Url> {
        let params = msg.params.as_ref()?;
        let text_document = params.get("textDocument")?;
        let uri_str = text_document.get("uri")?.as_str()?;
        url::Url::parse(uri_str).ok()
    }

    /// Get the venv path for a document URI from cache
    fn venv_for_uri(&self, url: &url::Url) -> Option<PathBuf> {
        self.state
            .open_documents
            .get(url)
            .and_then(|doc| doc.venv.clone())
    }

    /// Handle didOpen: cache document, ensure backend in pool, forward
    async fn handle_did_open(
        &mut self,
        msg: &RpcMessage,
        count: usize,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<(), ProxyError> {
        if let Some(params) = &msg.params {
            if let Some(text_document) = params.get("textDocument") {
                let text = text_document
                    .get("text")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string());

                if let Some(uri_value) = text_document.get("uri") {
                    if let Some(uri_str) = uri_value.as_str() {
                        if let Ok(url) = url::Url::parse(uri_str) {
                            if let Ok(file_path) = url.to_file_path() {
                                let language_id = text_document
                                    .get("languageId")
                                    .and_then(|l| l.as_str())
                                    .unwrap_or("unknown")
                                    .to_string();

                                let version = text_document
                                    .get("version")
                                    .and_then(|v| v.as_i64())
                                    .unwrap_or(0)
                                    as i32;

                                tracing::info!(
                                    count = count,
                                    uri = uri_str,
                                    path = %file_path.display(),
                                    "didOpen received"
                                );

                                // Search for .venv
                                let found_venv =
                                    venv::find_venv(&file_path, self.state.git_toplevel.as_deref())
                                        .await?;

                                // Cache document
                                if let Some(text_content) = &text {
                                    let doc = crate::state::OpenDocument {
                                        language_id: language_id.clone(),
                                        version,
                                        text: text_content.clone(),
                                        venv: found_venv.clone(),
                                    };
                                    self.state.open_documents.insert(url.clone(), doc);
                                }

                                // Ensure backend in pool and forward didOpen
                                if let Some(ref venv_path) = found_venv {
                                    if !self.state.pool.contains(venv_path) {
                                        // Need to create backend
                                        if self.state.pool.is_full() {
                                            self.evict_lru_backend(client_writer).await?;
                                        }

                                        match self
                                            .create_backend_instance(venv_path, client_writer)
                                            .await
                                        {
                                            Ok(instance) => {
                                                self.state.pool.insert(venv_path.clone(), instance);
                                                // didOpen was already restored during create_backend_instance
                                                // (restore_documents_to_backend sends didOpen for matching docs)
                                                return Ok(());
                                            }
                                            Err(e) => {
                                                tracing::error!(
                                                    venv = %venv_path.display(),
                                                    error = ?e,
                                                    "Failed to create backend for didOpen"
                                                );
                                                return Ok(());
                                            }
                                        }
                                    }

                                    // Backend exists in pool — forward didOpen
                                    if let Some(inst) = self.state.pool.get_mut(venv_path) {
                                        inst.last_used = Instant::now();
                                        if let Err(e) = inst.writer.write_message(msg).await {
                                            tracing::warn!(
                                                venv = %venv_path.display(),
                                                error = ?e,
                                                "Failed to forward didOpen to backend"
                                            );
                                        }
                                    }
                                } else {
                                    tracing::debug!(
                                        uri = uri_str,
                                        "No venv found for document, not forwarding didOpen"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Clear diagnostics for specified URIs (send empty array)
    async fn clear_diagnostics_for_uris(
        &self,
        uris: &[url::Url],
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> (usize, usize) {
        let mut ok = 0;
        let mut failed = 0;

        for uri in uris {
            tracing::trace!(uri = %uri, "Clearing diagnostics");

            let clear_msg = RpcMessage {
                jsonrpc: "2.0".to_string(),
                id: None,
                method: Some("textDocument/publishDiagnostics".to_string()),
                params: Some(serde_json::json!({
                    "uri": uri.to_string(),
                    "diagnostics": []
                })),
                result: None,
                error: None,
            };

            match client_writer.write_message(&clear_msg).await {
                Ok(_) => ok += 1,
                Err(e) => {
                    failed += 1;
                    tracing::warn!(uri = %uri, error = ?e, "Failed to clear diagnostics");
                }
            }
        }

        (ok, failed)
    }

    /// Handle didChange
    async fn handle_did_change(&mut self, msg: &RpcMessage) -> Result<(), ProxyError> {
        if let Some(params) = &msg.params {
            if let Some(text_document) = params.get("textDocument") {
                if let Some(uri_str) = text_document.get("uri").and_then(|u| u.as_str()) {
                    if let Ok(url) = url::Url::parse(uri_str) {
                        let version = text_document
                            .get("version")
                            .and_then(|v| v.as_i64())
                            .map(|v| v as i32);

                        if let Some(content_changes) = params.get("contentChanges") {
                            if let Some(changes_array) = content_changes.as_array() {
                                if changes_array.is_empty() {
                                    tracing::debug!(
                                        uri = %url,
                                        "didChange received with empty contentChanges, ignoring"
                                    );
                                    return Ok(());
                                }

                                if let Some(doc) = self.state.open_documents.get_mut(&url) {
                                    for change in changes_array {
                                        if let Some(range) = change.get("range") {
                                            if let Some(new_text) =
                                                change.get("text").and_then(|t| t.as_str())
                                            {
                                                crate::text_edit::apply_incremental_change(
                                                    &mut doc.text,
                                                    range,
                                                    new_text,
                                                )?;
                                            }
                                        } else if let Some(new_text) =
                                            change.get("text").and_then(|t| t.as_str())
                                        {
                                            doc.text = new_text.to_string();
                                        }
                                    }

                                    if let Some(v) = version {
                                        doc.version = v;
                                    }

                                    tracing::debug!(
                                        uri = %url,
                                        version = doc.version,
                                        text_len = doc.text.len(),
                                        "Document text updated"
                                    );
                                } else {
                                    tracing::warn!(
                                        uri = %url,
                                        "didChange for unopened document, ignoring"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Handle didClose: remove document from cache
    async fn handle_did_close(&mut self, msg: &RpcMessage) -> Result<(), ProxyError> {
        if let Some(params) = &msg.params {
            if let Some(text_document) = params.get("textDocument") {
                if let Some(uri_str) = text_document.get("uri").and_then(|u| u.as_str()) {
                    if let Ok(url) = url::Url::parse(uri_str) {
                        if self.state.open_documents.remove(&url).is_some() {
                            tracing::debug!(
                                uri = %url,
                                remaining_docs = self.state.open_documents.len(),
                                "Document removed from cache"
                            );
                        } else {
                            tracing::warn!(
                                uri = %url,
                                "didClose for unknown document"
                            );
                        }
                    }
                }
            }
        }

        Ok(())
    }
}
