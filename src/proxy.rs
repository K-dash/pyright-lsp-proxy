use crate::backend::PyrightBackend;
use crate::backend_state::BackendState;
use crate::error::ProxyError;
use crate::framing::{LspFrameReader, LspFrameWriter};
use crate::message::RpcMessage;
use crate::state::ProxyState;
use crate::venv;
use tokio::io::{stdin, stdout};

pub struct LspProxy {
    state: ProxyState,
}

impl LspProxy {
    pub fn new() -> Self {
        Self {
            state: ProxyState::new(),
        }
    }

    pub async fn run(&mut self) -> Result<(), ProxyError> {
        // Frame reader/writer for stdin/stdout
        let mut client_reader = LspFrameReader::new(stdin());
        let mut client_writer = LspFrameWriter::new(stdout());

        // Get cwd at startup
        let cwd = std::env::current_dir()?;
        tracing::info!(cwd = %cwd.display(), "Starting pyright-lsp-proxy");

        // Get and cache git toplevel
        self.state.git_toplevel = venv::get_git_toplevel(&cwd).await?;

        // Search for fallback env
        let fallback_venv = venv::find_fallback_venv(&cwd).await?;

        // Start backend (with fallback env, or without venv if not found)
        let mut backend_state = if let Some(venv) = fallback_venv {
            tracing::info!(venv = %venv.display(), "Using fallback .venv");
            let backend = PyrightBackend::spawn(Some(&venv)).await?;
            BackendState::Running {
                backend: Box::new(backend),
                active_venv: venv,
                session: self.state.backend_session,
            }
        } else {
            tracing::warn!("No fallback .venv found, starting in Disabled mode (strict venv)");
            // Start in Disabled state when no venv found (strict mode)
            // Don't spawn backend (spawn when venv is found on didOpen)
            BackendState::Disabled {
                reason: "No fallback .venv found".to_string(),
                last_file: None,
            }
        };

        let mut didopen_count = 0;

        loop {
            tokio::select! {
                // Messages from client (Claude Code)
                result = client_reader.read_message() => {
                    let msg = result?;
                    let method = msg.method_name();
                    let is_disabled = backend_state.is_disabled();

                    tracing::debug!(
                        method = ?method,
                        is_request = msg.is_request(),
                        is_notification = msg.is_notification(),
                        "Client -> Proxy"
                    );

                    // Cache initialize
                    if method == Some("initialize") {
                        tracing::info!("Caching initialize message for backend restart");
                        self.state.client_initialize = Some(msg.clone());

                        // Return success response even in Disabled state (with empty capabilities)
                        if is_disabled {
                            tracing::warn!("Disabled mode: returning minimal initialize response");
                            let init_response = crate::message::RpcMessage {
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
                            continue;
                        }
                    }

                    // initialized notification (ignored in Disabled state)
                    if method == Some("initialized") && is_disabled {
                        tracing::debug!("Disabled mode: ignoring initialized notification");
                        continue;
                    }

                    // 1. Always process didOpen (revival trigger)
                    if method == Some("textDocument/didOpen") {
                        didopen_count += 1;
                        self.handle_did_open(&msg, didopen_count, &mut backend_state, &mut client_writer).await?;
                        continue; // didOpen already handled
                    }

                    // 2. Always update cache for didChange/didClose
                    if method == Some("textDocument/didChange") {
                        self.handle_did_change(&msg).await?;
                        if is_disabled { continue; }  // Don't send to backend when Disabled
                    }
                    if method == Some("textDocument/didClose") {
                        self.handle_did_close(&msg).await?;
                        if is_disabled { continue; }  // Don't send to backend when Disabled
                    }

                    // 3. Request processing (with transparent retry support)
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

                        // Pass through ensure_backend_for_uri for VENV_CHECK_METHODS
                        if let Some(method_name) = m {
                            if VENV_CHECK_METHODS.contains(&method_name) {
                                if let Some(url) = Self::extract_text_document_uri(&msg) {
                                    if let Ok(file_path) = url.to_file_path() {
                                        let old_session = backend_state.session();

                                        let switched = self
                                            .ensure_backend_for_uri(
                                                &mut backend_state,
                                                &mut client_writer,
                                                &url,
                                                &file_path,
                                            )
                                            .await?;

                                        if switched {
                                            tracing::info!(
                                                method = method_name,
                                                uri = %url,
                                                from_session = ?old_session,
                                                to_session = ?backend_state.session(),
                                                "Venv switched, request will be sent to new backend"
                                            );
                                        }
                                    } else {
                                        // URI extraction succeeded but file_path conversion failed
                                        tracing::debug!(
                                            method = method_name,
                                            uri = %url,
                                            "Skipping venv check: could not convert URI to file path"
                                        );
                                    }
                                } else {
                                    // URI extraction failed → skip switch check
                                    tracing::debug!(
                                        method = method_name,
                                        "Skipping venv check: could not extract textDocument.uri"
                                    );
                                }
                            }
                        }

                        // Register in pending (record current backend session)
                        if let Some(id) = &msg.id {
                            if let Some(session) = backend_state.session() {
                                self.state.pending_requests.insert(
                                    id.clone(),
                                    crate::state::PendingRequest {
                                        backend_session: session,
                                    },
                                );
                            }
                        }
                    }

                    // 4. Return error for requests in Disabled state (except VENV_CHECK_METHODS)
                    // Re-check since state may have changed in ensure_backend_for_uri()
                    let is_disabled = backend_state.is_disabled();
                    if is_disabled && msg.is_request() {
                        let error_message = "pyright-lsp-proxy: .venv not found (strict mode). Create .venv or run hooks.";
                        if let Some((reason, last_file)) = backend_state.disabled_info() {
                            tracing::warn!(
                                method = ?method,
                                reason = reason,
                                last_file = ?last_file.map(|p| p.display().to_string()),
                                error_message = error_message,
                                "Returning error response to client (Disabled mode)"
                            );
                        }
                        let error_response = create_error_response(&msg, error_message);
                        client_writer.write_message(&error_response).await?;
                        continue;
                    }

                    // 5. Forward to backend when Running
                    if let BackendState::Running { backend, session, .. } = &mut backend_state {
                        if msg.is_request() {
                            tracing::debug!(
                                session = *session,
                                method = msg.method.as_deref(),
                                "Sending request to backend"
                            );
                        }
                        backend.send_message(&msg).await?;
                    }
                }

                // Wait for backend read only when Running
                result = async {
                    match &mut backend_state {
                        BackendState::Running { backend, .. } => backend.read_message().await,
                        BackendState::Disabled { .. } => std::future::pending().await,
                    }
                } => {
                    let msg = result?;
                    let running_session = backend_state.session();

                    tracing::debug!(
                        is_response = msg.is_response(),
                        is_notification = msg.is_notification(),
                        "Backend -> Proxy"
                    );

                    // Resolve pending with backend response + generation check
                    if msg.is_response() {
                        if let Some(id) = &msg.id {
                            // Get from pending and check generation
                            if let Some(pending) = self.state.pending_requests.get(id) {
                                if Some(pending.backend_session) != running_session {
                                    // Response from old generation → discard
                                    tracing::warn!(
                                        id = ?id,
                                        pending_session = pending.backend_session,
                                        running_session = ?running_session,
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
            }
        }
    }

    /// Extract textDocument.uri from LSP request params
    fn extract_text_document_uri(msg: &RpcMessage) -> Option<url::Url> {
        let params = msg.params.as_ref()?;
        let text_document = params.get("textDocument")?;
        let uri_str = text_document.get("uri")?.as_str()?;
        url::Url::parse(uri_str).ok()
    }

    /// Ensure appropriate backend for URI
    /// Returns: whether a switch occurred
    async fn ensure_backend_for_uri(
        &mut self,
        backend_state: &mut BackendState,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
        url: &url::Url,
        file_path: &std::path::Path,
    ) -> Result<bool, ProxyError> {
        // Get venv from cache (O(1))
        let target_venv = if let Some(doc) = self.state.open_documents.get(url) {
            doc.venv.clone()
        } else {
            // URI without didOpen → search (exceptional path)
            tracing::debug!(uri = %url, "URI not in cache, searching venv");
            venv::find_venv(file_path, self.state.git_toplevel.as_deref()).await?
        };

        // State transition via common function
        self.transition_backend_state(
            backend_state,
            client_writer,
            target_venv.as_deref(),
            file_path,
        )
        .await
    }

    /// Transition backend state based on venv
    /// Returns: whether a switch occurred
    async fn transition_backend_state(
        &mut self,
        backend_state: &mut BackendState,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
        target_venv: Option<&std::path::Path>,
        trigger_file: &std::path::Path,
    ) -> Result<bool, ProxyError> {
        match (&*backend_state, target_venv) {
            // Running + same venv → do nothing
            (BackendState::Running { active_venv, .. }, Some(venv)) if active_venv == venv => {
                tracing::debug!(venv = %venv.display(), "Using same .venv as before");
                Ok(false)
            }

            // Running + different venv → switch
            (BackendState::Running { .. }, Some(venv)) => {
                let old_session = backend_state.session().unwrap_or(0);

                tracing::warn!(
                    current = ?backend_state.active_venv().map(|p| p.display().to_string()),
                    found = %venv.display(),
                    "Venv switch needed, restarting backend"
                );

                if let BackendState::Running { backend, .. } = backend_state {
                    // Cancel pending requests for old session
                    self.cancel_pending_requests_for_session(client_writer, old_session)
                        .await?;

                    let new_backend = self
                        .restart_backend_with_venv(backend, venv, client_writer)
                        .await?;
                    *backend_state = BackendState::Running {
                        backend: Box::new(new_backend),
                        active_venv: venv.to_path_buf(),
                        session: self.state.backend_session,
                    };
                }
                Ok(true)
            }

            // Running + venv not found → transition to Disabled
            (BackendState::Running { .. }, None) => {
                tracing::warn!(
                    path = %trigger_file.display(),
                    "No .venv found for this file, disabling backend"
                );

                if let BackendState::Running { backend, .. } = backend_state {
                    self.disable_backend(backend, client_writer, trigger_file)
                        .await?;
                    *backend_state = BackendState::Disabled {
                        reason: format!("No .venv found for file: {}", trigger_file.display()),
                        last_file: Some(trigger_file.to_path_buf()),
                    };
                }
                Ok(true)
            }

            // Disabled + venv found → revive to Running
            (BackendState::Disabled { .. }, Some(venv)) => {
                tracing::info!(venv = %venv.display(), "Found .venv, spawning backend");
                let new_backend = self.spawn_and_init_backend(venv, client_writer).await?;
                *backend_state = BackendState::Running {
                    backend: Box::new(new_backend),
                    active_venv: venv.to_path_buf(),
                    session: self.state.backend_session,
                };
                Ok(true)
            }

            // Disabled + venv not found → stay as is
            (BackendState::Disabled { reason, last_file }, None) => {
                tracing::warn!(
                    path = %trigger_file.display(),
                    reason = reason.as_str(),
                    last_file = ?last_file.as_ref().map(|p| p.display().to_string()),
                    "No .venv found for this file (backend still disabled)"
                );
                Ok(false)
            }
        }
    }

    /// Handle didOpen & .venv switch decision + BackendState transition (Strict venv mode)
    async fn handle_did_open(
        &mut self,
        msg: &crate::message::RpcMessage,
        count: usize,
        backend_state: &mut BackendState,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<(), ProxyError> {
        // Extract URI and text from params
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
                                // Get languageId and version
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
                                    has_text = text.is_some(),
                                    text_len = text.as_ref().map(|s| s.len()).unwrap_or(0),
                                    language_id = %language_id,
                                    version = version,
                                    "didOpen received"
                                );

                                // Search for .venv
                                let found_venv =
                                    venv::find_venv(&file_path, self.state.git_toplevel.as_deref())
                                        .await?;

                                // Cache didOpen (for revival when Disabled)
                                if let Some(text_content) = &text {
                                    let doc = crate::state::OpenDocument {
                                        language_id: language_id.clone(),
                                        version,
                                        text: text_content.clone(),
                                        venv: found_venv.clone(),
                                    };
                                    self.state.open_documents.insert(url.clone(), doc);
                                    tracing::debug!(
                                        uri = %url,
                                        doc_count = self.state.open_documents.len(),
                                        cached_venv = ?found_venv.as_ref().map(|p| p.display().to_string()),
                                        "Document cached"
                                    );
                                }

                                // State transition logic (Strict venv mode)
                                tracing::debug!(
                                    is_running = !backend_state.is_disabled(),
                                    is_disabled = backend_state.is_disabled(),
                                    has_venv = found_venv.is_some(),
                                    "State transition check"
                                );

                                self.transition_backend_state(
                                    backend_state,
                                    client_writer,
                                    found_venv.as_deref(),
                                    &file_path,
                                )
                                .await?;
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Gracefully shutdown backend and restart with new .venv
    async fn restart_backend_with_venv(
        &mut self,
        backend: &mut PyrightBackend,
        new_venv: &std::path::Path,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<PyrightBackend, ProxyError> {
        self.state.backend_session += 1;
        let session = self.state.backend_session;

        tracing::info!(
            session = session,
            new_venv = %new_venv.display(),
            "Starting backend restart sequence"
        );

        // 1. Shutdown existing backend
        if let Err(e) = backend.shutdown_gracefully().await {
            tracing::error!(error = ?e, "Failed to shutdown backend gracefully");
            // Continue even on error (try to start new backend)
        }

        // 2. Start new backend
        tracing::info!(session = session, venv = %new_venv.display(), "Spawning new backend");
        let mut new_backend = PyrightBackend::spawn(Some(new_venv)).await?;

        // 3. Send initialize to backend (proxy becomes backend client)
        let init_params = self
            .state
            .client_initialize
            .as_ref()
            .and_then(|msg| msg.params.clone())
            .ok_or_else(|| ProxyError::InvalidMessage("No initialize params cached".to_string()))?;

        let init_msg = crate::message::RpcMessage {
            jsonrpc: "2.0".to_string(),
            id: Some(crate::message::RpcId::Number(1)),
            method: Some("initialize".to_string()),
            params: Some(init_params),
            result: None,
            error: None,
        };

        tracing::info!(session = session, "Sending initialize to new backend");
        new_backend.send_message(&init_msg).await?;

        // 4. Receive initialize response (skip notifications, check id, with timeout)
        let init_id = 1i64;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(ProxyError::Backend(
                    crate::error::BackendError::InitializeTimeout(10),
                ));
            }

            let wait_result = tokio::time::timeout(remaining, new_backend.read_message()).await;

            match wait_result {
                Ok(Ok(msg)) => {
                    if msg.is_response() {
                        // Check if id matches
                        if let Some(crate::message::RpcId::Number(id)) = &msg.id {
                            if *id == init_id {
                                // Check if error response
                                if let Some(error) = &msg.error {
                                    return Err(ProxyError::Backend(
                                        crate::error::BackendError::InitializeResponseError(
                                            format!(
                                                "code={}, message={}",
                                                error.code, error.message
                                            ),
                                        ),
                                    ));
                                }

                                tracing::info!(
                                    session = session,
                                    response_id = ?msg.id,
                                    "Received initialize response from backend"
                                );

                                // Log textDocumentSync capability
                                if let Some(result) = &msg.result {
                                    if let Some(capabilities) = result.get("capabilities") {
                                        if let Some(sync) = capabilities.get("textDocumentSync") {
                                            tracing::info!(
                                                session = session,
                                                text_document_sync = ?sync,
                                                "Backend textDocumentSync capability"
                                            );
                                        }
                                    }
                                }

                                break;
                            } else {
                                tracing::debug!(
                                    session = session,
                                    response_id = ?msg.id,
                                    expected_id = init_id,
                                    "Received different response, continuing"
                                );
                            }
                        }
                    } else {
                        // Ignore notifications and continue loop
                        tracing::debug!(
                            session = session,
                            method = ?msg.method,
                            "Received notification during initialize, ignoring"
                        );
                    }
                }
                Ok(Err(e)) => {
                    return Err(ProxyError::Backend(
                        crate::error::BackendError::InitializeFailed(format!(
                            "Error reading initialize response: {}",
                            e
                        )),
                    ));
                }
                Err(_) => {
                    return Err(ProxyError::Backend(
                        crate::error::BackendError::InitializeTimeout(10),
                    ));
                }
            }
        }

        // 5. Send initialized notification
        let initialized_msg = crate::message::RpcMessage {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: Some("initialized".to_string()),
            params: Some(serde_json::json!({})),
            result: None,
            error: None,
        };

        tracing::info!(session = session, "Sending initialized to backend");
        new_backend.send_message(&initialized_msg).await?;

        // 6. Document restoration
        // Restore only documents under the new venv's parent directory
        let venv_parent = new_venv.parent().map(|p| p.to_path_buf());
        let total_docs = self.state.open_documents.len();
        let mut restored = 0;
        let mut skipped = 0;
        let mut failed = 0;
        let mut skipped_uris: Vec<url::Url> = Vec::new();

        tracing::info!(
            session = session,
            total_docs = total_docs,
            venv_parent = ?venv_parent.as_ref().map(|p| p.display().to_string()),
            "Starting document restoration"
        );

        for (url, doc) in &self.state.open_documents {
            // Restore only documents under venv's parent directory
            let should_restore = match (url.to_file_path().ok(), &venv_parent) {
                (Some(file_path), Some(venv_parent)) => file_path.starts_with(venv_parent),
                _ => false, // Skip if not file:// URL or venv_parent is None
            };

            if !should_restore {
                skipped += 1;
                skipped_uris.push(url.clone());
                tracing::debug!(
                    session = session,
                    uri = %url,
                    "Skipping document from different venv"
                );
                continue;
            }
            // Copy required values first (end borrow before await)
            let uri_str = url.to_string();
            let language_id = doc.language_id.clone();
            let version = doc.version;
            let text = doc.text.clone();
            let text_len = text.len();

            let didopen_msg = crate::message::RpcMessage {
                jsonrpc: "2.0".to_string(),
                id: None,
                method: Some("textDocument/didOpen".to_string()),
                params: Some(serde_json::json!({
                    "textDocument": {
                        "uri": uri_str,
                        "languageId": language_id,
                        "version": version,
                        "text": text,
                    }
                })),
                result: None,
                error: None,
            };

            match new_backend.send_message(&didopen_msg).await {
                Ok(_) => {
                    restored += 1;
                    tracing::info!(
                        session = session,
                        uri = %uri_str,
                        version = version,
                        text_len = text_len,
                        "Successfully restored document"
                    );
                }
                Err(e) => {
                    failed += 1;
                    tracing::error!(
                        session = session,
                        uri = %uri_str,
                        error = ?e,
                        "Failed to restore document, skipping"
                    );
                    // Continue with next document (partial restoration strategy)
                }
            }
        }

        tracing::info!(
            session = session,
            restored = restored,
            skipped = skipped,
            failed = failed,
            total = total_docs,
            "Document restoration completed"
        );

        // Clear diagnostics for skipped URIs
        if !skipped_uris.is_empty() {
            let (ok, clear_failed) = self
                .clear_diagnostics_for_uris(&skipped_uris, client_writer)
                .await;

            if clear_failed == 0 {
                tracing::info!(
                    session = session,
                    cleared_ok = ok,
                    "Diagnostics cleared for skipped documents"
                );
            } else {
                tracing::info!(
                    session = session,
                    cleared_ok = ok,
                    cleared_failed = clear_failed,
                    "Diagnostics clear partially failed for skipped documents"
                );
            }
        }

        tracing::info!(
            session = session,
            venv = %new_venv.display(),
            "Backend restart completed successfully"
        );

        Ok(new_backend)
    }

    /// Clear diagnostics for specified URIs (send empty array)
    /// Best effort: continue even if one fails
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

    /// Shutdown backend and transition to Disabled state (Strict venv mode)
    async fn disable_backend(
        &mut self,
        backend: &mut PyrightBackend,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
        file_path: &std::path::Path,
    ) -> Result<(), ProxyError> {
        self.state.backend_session += 1;
        let session = self.state.backend_session;

        tracing::info!(
            session = session,
            file = %file_path.display(),
            "Disabling backend (no .venv found)"
        );

        // Send empty diagnostics to all URIs in open_documents (clone first to avoid borrow issues)
        let uris: Vec<url::Url> = self.state.open_documents.keys().cloned().collect();
        let (ok, failed) = self.clear_diagnostics_for_uris(&uris, client_writer).await;

        if failed == 0 {
            tracing::info!(
                session = session,
                cleared_ok = ok,
                "Diagnostics cleared for all open documents"
            );
        } else {
            tracing::info!(
                session = session,
                cleared_ok = ok,
                cleared_failed = failed,
                "Diagnostics clear partially failed"
            );
        }

        // Return RequestCancelled to unresolved requests
        self.cancel_pending_requests(client_writer).await?;

        // Shutdown backend
        if let Err(e) = backend.shutdown_gracefully().await {
            tracing::error!(error = ?e, "Failed to shutdown backend gracefully");
        }

        tracing::info!(session = session, "Backend disabled");

        Ok(())
    }

    /// Spawn and initialize backend (for Disabled → Running revival)
    async fn spawn_and_init_backend(
        &mut self,
        venv: &std::path::Path,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<PyrightBackend, ProxyError> {
        self.state.backend_session += 1;
        let session = self.state.backend_session;

        tracing::info!(
            session = session,
            venv = %venv.display(),
            "Spawning backend from Disabled state"
        );

        // 1. Start new backend
        let mut new_backend = PyrightBackend::spawn(Some(venv)).await?;

        // 2. Send initialize to backend
        let init_params = self
            .state
            .client_initialize
            .as_ref()
            .and_then(|msg| msg.params.clone())
            .ok_or_else(|| ProxyError::InvalidMessage("No initialize params cached".to_string()))?;

        let init_msg = crate::message::RpcMessage {
            jsonrpc: "2.0".to_string(),
            id: Some(crate::message::RpcId::Number(1)),
            method: Some("initialize".to_string()),
            params: Some(init_params),
            result: None,
            error: None,
        };

        tracing::info!(session = session, "Sending initialize to new backend");
        new_backend.send_message(&init_msg).await?;

        // 3. Receive initialize response
        let init_id = 1i64;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(ProxyError::Backend(
                    crate::error::BackendError::InitializeTimeout(10),
                ));
            }

            let wait_result = tokio::time::timeout(remaining, new_backend.read_message()).await;

            match wait_result {
                Ok(Ok(msg)) => {
                    if msg.is_response() {
                        if let Some(crate::message::RpcId::Number(id)) = &msg.id {
                            if *id == init_id {
                                if let Some(error) = &msg.error {
                                    return Err(ProxyError::Backend(
                                        crate::error::BackendError::InitializeResponseError(
                                            format!(
                                                "code={}, message={}",
                                                error.code, error.message
                                            ),
                                        ),
                                    ));
                                }

                                tracing::info!(
                                    session = session,
                                    response_id = ?msg.id,
                                    "Received initialize response from backend"
                                );

                                break;
                            }
                        }
                    } else {
                        tracing::debug!(
                            session = session,
                            method = ?msg.method,
                            "Received notification during initialize, ignoring"
                        );
                    }
                }
                Ok(Err(e)) => {
                    return Err(ProxyError::Backend(
                        crate::error::BackendError::InitializeFailed(format!(
                            "Error reading initialize response: {}",
                            e
                        )),
                    ));
                }
                Err(_) => {
                    return Err(ProxyError::Backend(
                        crate::error::BackendError::InitializeTimeout(10),
                    ));
                }
            }
        }

        // 4. Send initialized notification
        let initialized_msg = crate::message::RpcMessage {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: Some("initialized".to_string()),
            params: Some(serde_json::json!({})),
            result: None,
            error: None,
        };

        tracing::info!(session = session, "Sending initialized to backend");
        new_backend.send_message(&initialized_msg).await?;

        // 5. Document restoration (only under venv's parent directory)
        let venv_parent = venv.parent().map(|p| p.to_path_buf());
        let total_docs = self.state.open_documents.len();
        let mut restored = 0;
        let mut skipped = 0;
        let mut failed = 0;
        let mut skipped_uris: Vec<url::Url> = Vec::new();

        tracing::info!(
            session = session,
            total_docs = total_docs,
            venv_parent = ?venv_parent.as_ref().map(|p| p.display().to_string()),
            "Starting document restoration"
        );

        for (url, doc) in &self.state.open_documents {
            let should_restore = match (url.to_file_path().ok(), &venv_parent) {
                (Some(file_path), Some(venv_parent)) => file_path.starts_with(venv_parent),
                _ => false,
            };

            if !should_restore {
                skipped += 1;
                skipped_uris.push(url.clone());
                tracing::debug!(
                    session = session,
                    uri = %url,
                    "Skipping document from different venv"
                );
                continue;
            }

            let uri_str = url.to_string();
            let language_id = doc.language_id.clone();
            let version = doc.version;
            let text = doc.text.clone();
            let text_len = text.len();

            let didopen_msg = crate::message::RpcMessage {
                jsonrpc: "2.0".to_string(),
                id: None,
                method: Some("textDocument/didOpen".to_string()),
                params: Some(serde_json::json!({
                    "textDocument": {
                        "uri": uri_str,
                        "languageId": language_id,
                        "version": version,
                        "text": text,
                    }
                })),
                result: None,
                error: None,
            };

            match new_backend.send_message(&didopen_msg).await {
                Ok(_) => {
                    restored += 1;
                    tracing::info!(
                        session = session,
                        uri = %uri_str,
                        version = version,
                        text_len = text_len,
                        "Successfully restored document"
                    );
                }
                Err(e) => {
                    failed += 1;
                    tracing::error!(
                        session = session,
                        uri = %uri_str,
                        error = ?e,
                        "Failed to restore document, skipping"
                    );
                }
            }
        }

        tracing::info!(
            session = session,
            restored = restored,
            skipped = skipped,
            failed = failed,
            total = total_docs,
            "Document restoration completed"
        );

        // Clear diagnostics for skipped URIs
        if !skipped_uris.is_empty() {
            let (ok, clear_failed) = self
                .clear_diagnostics_for_uris(&skipped_uris, client_writer)
                .await;

            if clear_failed == 0 {
                tracing::info!(
                    session = session,
                    cleared_ok = ok,
                    "Diagnostics cleared for skipped documents"
                );
            } else {
                tracing::info!(
                    session = session,
                    cleared_ok = ok,
                    cleared_failed = clear_failed,
                    "Diagnostics clear partially failed for skipped documents"
                );
            }
        }

        tracing::info!(
            session = session,
            venv = %venv.display(),
            "Backend spawned and initialized successfully"
        );

        Ok(new_backend)
    }

    /// Return RequestCancelled to unresolved requests
    async fn cancel_pending_requests(
        &mut self,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<(), ProxyError> {
        const REQUEST_CANCELLED: i64 = -32800;
        let pending: Vec<_> = self
            .state
            .pending_requests
            .drain()
            .map(|(id, _)| id)
            .collect();

        for id in pending {
            let msg = crate::message::RpcMessage {
                jsonrpc: "2.0".to_string(),
                id: Some(id),
                method: None,
                params: None,
                result: None,
                error: Some(crate::message::RpcError {
                    code: REQUEST_CANCELLED,
                    message: "Request cancelled".to_string(),
                    data: None,
                }),
            };

            client_writer.write_message(&msg).await?;
        }

        Ok(())
    }

    /// Cancel pending requests for specified session
    async fn cancel_pending_requests_for_session(
        &mut self,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
        old_session: u64,
    ) -> Result<(), ProxyError> {
        const REQUEST_CANCELLED: i64 = -32800;

        let to_cancel: Vec<_> = self
            .state
            .pending_requests
            .iter()
            .filter(|(_, p)| p.backend_session == old_session)
            .map(|(id, _)| id.clone())
            .collect();

        for id in to_cancel {
            self.state.pending_requests.remove(&id);
            let msg = crate::message::RpcMessage {
                jsonrpc: "2.0".to_string(),
                id: Some(id.clone()),
                method: None,
                params: None,
                result: None,
                error: Some(crate::message::RpcError {
                    code: REQUEST_CANCELLED,
                    message: "Request cancelled due to backend restart".to_string(),
                    data: None,
                }),
            };
            client_writer.write_message(&msg).await?;
            tracing::info!(id = ?id, session = old_session, "Cancelled pending request");
        }

        Ok(())
    }

    /// Handle didChange
    async fn handle_did_change(
        &mut self,
        msg: &crate::message::RpcMessage,
    ) -> Result<(), ProxyError> {
        if let Some(params) = &msg.params {
            if let Some(text_document) = params.get("textDocument") {
                if let Some(uri_str) = text_document.get("uri").and_then(|u| u.as_str()) {
                    if let Ok(url) = url::Url::parse(uri_str) {
                        // Get version from textDocument (trust LSP version)
                        let version = text_document
                            .get("version")
                            .and_then(|v| v.as_i64())
                            .map(|v| v as i32);

                        // Get text from contentChanges
                        if let Some(content_changes) = params.get("contentChanges") {
                            if let Some(changes_array) = content_changes.as_array() {
                                // Check for empty contentChanges
                                if changes_array.is_empty() {
                                    tracing::debug!(
                                        uri = %url,
                                        "didChange received with empty contentChanges, ignoring"
                                    );
                                    return Ok(());
                                }

                                // Update only if document exists
                                if let Some(doc) = self.state.open_documents.get_mut(&url) {
                                    // Apply each change in order
                                    for change in changes_array {
                                        if let Some(range) = change.get("range") {
                                            // Incremental sync: partial update using range
                                            if let Some(new_text) =
                                                change.get("text").and_then(|t| t.as_str())
                                            {
                                                Self::apply_incremental_change(
                                                    &mut doc.text,
                                                    range,
                                                    new_text,
                                                )?;
                                                tracing::debug!(
                                                    uri = %url,
                                                    "Applied incremental change"
                                                );
                                            }
                                        } else {
                                            // Full sync: replace entire text
                                            if let Some(new_text) =
                                                change.get("text").and_then(|t| t.as_str())
                                            {
                                                doc.text = new_text.to_string();
                                                tracing::debug!(
                                                    uri = %url,
                                                    text_len = new_text.len(),
                                                    "Applied full sync change"
                                                );
                                            }
                                        }
                                    }

                                    // Adopt LSP version
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
    async fn handle_did_close(
        &mut self,
        msg: &crate::message::RpcMessage,
    ) -> Result<(), ProxyError> {
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

    /// Apply incremental change (range-based partial replacement)
    fn apply_incremental_change(
        text: &mut String,
        range: &serde_json::Value,
        new_text: &str,
    ) -> Result<(), ProxyError> {
        // Get start/end from range
        let start = range.get("start").ok_or_else(|| {
            ProxyError::InvalidMessage("didChange range missing start".to_string())
        })?;
        let end = range
            .get("end")
            .ok_or_else(|| ProxyError::InvalidMessage("didChange range missing end".to_string()))?;

        let start_line =
            start.get("line").and_then(|l| l.as_u64()).ok_or_else(|| {
                ProxyError::InvalidMessage("didChange start missing line".to_string())
            })? as usize;
        let start_char = start
            .get("character")
            .and_then(|c| c.as_u64())
            .ok_or_else(|| {
                ProxyError::InvalidMessage("didChange start missing character".to_string())
            })? as usize;

        let end_line =
            end.get("line").and_then(|l| l.as_u64()).ok_or_else(|| {
                ProxyError::InvalidMessage("didChange end missing line".to_string())
            })? as usize;
        let end_char = end
            .get("character")
            .and_then(|c| c.as_u64())
            .ok_or_else(|| {
                ProxyError::InvalidMessage("didChange end missing character".to_string())
            })? as usize;

        // Convert line/character to byte offset
        let start_offset = Self::position_to_offset(text, start_line, start_char)?;
        let end_offset = Self::position_to_offset(text, end_line, end_char)?;

        // Validate range (start > end is invalid)
        if start_offset > end_offset {
            return Err(ProxyError::InvalidMessage(format!(
                "Invalid range: start offset ({}) > end offset ({})",
                start_offset, end_offset
            )));
        }

        // Replace range
        text.replace_range(start_offset..end_offset, new_text);

        Ok(())
    }

    /// Convert LSP position (line, character) to byte offset
    /// LSP character is UTF-16 code unit count
    fn position_to_offset(text: &str, line: usize, character: usize) -> Result<usize, ProxyError> {
        let mut current_line = 0;
        let mut line_start_offset = 0;

        for (idx, ch) in text.char_indices() {
            if ch == '\n' {
                if current_line == line {
                    // Reached end of target line (before newline character)
                    return Self::find_offset_in_line(text, line_start_offset, idx, character);
                }
                current_line += 1;
                line_start_offset = idx + 1;
            }
        }

        // Last line (if not ending with newline) or first line of empty text
        if current_line == line {
            return Self::find_offset_in_line(text, line_start_offset, text.len(), character);
        }

        // Line number out of range
        Err(ProxyError::InvalidMessage(format!(
            "Position out of range: line={} (max={}), character={}",
            line, current_line, character
        )))
    }

    /// Count UTF-16 code units within line and return byte offset
    /// Clamp to end of line if character exceeds line length
    fn find_offset_in_line(
        text: &str,
        line_start: usize,
        line_end: usize,
        character: usize,
    ) -> Result<usize, ProxyError> {
        let line_text = &text[line_start..line_end];
        let mut utf16_offset = 0;

        for (idx, ch) in line_text.char_indices() {
            if utf16_offset >= character {
                return Ok(line_start + idx);
            }
            utf16_offset += ch.len_utf16();
        }

        // Clamp to end of line if character exceeds line length
        Ok(line_end)
    }
}

/// Create error response (returned for requests in Disabled state)
fn create_error_response(request: &RpcMessage, message: &str) -> RpcMessage {
    RpcMessage {
        jsonrpc: "2.0".to_string(),
        id: request.id.clone(),
        method: None,
        params: None,
        result: None,
        error: Some(crate::message::RpcError {
            code: -32603, // Internal error (for compatibility)
            message: message.to_string(),
            data: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_position_to_offset_simple() {
        let text = "hello\nworld\n";

        // line 0, char 0 -> offset 0
        assert_eq!(LspProxy::position_to_offset(text, 0, 0).unwrap(), 0);

        // line 0, char 5 -> offset 5 (end of "hello")
        assert_eq!(LspProxy::position_to_offset(text, 0, 5).unwrap(), 5);

        // line 1, char 0 -> offset 6 (start of "world")
        assert_eq!(LspProxy::position_to_offset(text, 1, 0).unwrap(), 6);

        // line 1, char 5 -> offset 11 (end of "world")
        assert_eq!(LspProxy::position_to_offset(text, 1, 5).unwrap(), 11);
    }

    #[test]
    fn test_position_to_offset_multibyte() {
        // Text containing multibyte characters (Japanese: "hello")
        let text = "こんにちは\nworld\n";

        // line 0, char 0 -> offset 0
        assert_eq!(LspProxy::position_to_offset(text, 0, 0).unwrap(), 0);

        // line 0, char 1 -> offset 3 (after first Japanese character)
        assert_eq!(LspProxy::position_to_offset(text, 0, 1).unwrap(), 3);

        // line 1, char 0 -> offset 16 (start of "world", after Japanese text + newline)
        assert_eq!(LspProxy::position_to_offset(text, 1, 0).unwrap(), 16);
    }

    #[test]
    fn test_apply_incremental_change_simple_replace() {
        let mut text = "hello world".to_string();
        let range = json!({
            "start": { "line": 0, "character": 0 },
            "end": { "line": 0, "character": 5 }
        });

        LspProxy::apply_incremental_change(&mut text, &range, "hi").unwrap();
        assert_eq!(text, "hi world");
    }

    #[test]
    fn test_apply_incremental_change_insert() {
        let mut text = "hello world".to_string();
        let range = json!({
            "start": { "line": 0, "character": 5 },
            "end": { "line": 0, "character": 5 }
        });

        // Insert (empty range)
        LspProxy::apply_incremental_change(&mut text, &range, " beautiful").unwrap();
        assert_eq!(text, "hello beautiful world");
    }

    #[test]
    fn test_apply_incremental_change_delete() {
        let mut text = "hello beautiful world".to_string();
        let range = json!({
            "start": { "line": 0, "character": 5 },
            "end": { "line": 0, "character": 15 }
        });

        // Delete (empty new_text)
        LspProxy::apply_incremental_change(&mut text, &range, "").unwrap();
        assert_eq!(text, "hello world");
    }

    #[test]
    fn test_apply_incremental_change_multiline() {
        let mut text = "def hello():\n    print('hello')\n".to_string();
        let range = json!({
            "start": { "line": 1, "character": 11 },
            "end": { "line": 1, "character": 16 }
        });

        // Replace "hello" with "world"
        LspProxy::apply_incremental_change(&mut text, &range, "world").unwrap();
        assert_eq!(text, "def hello():\n    print('world')\n");
    }

    #[test]
    fn test_apply_incremental_change_cross_line() {
        let mut text = "line1\nline2\nline3\n".to_string();
        let range = json!({
            "start": { "line": 0, "character": 5 },
            "end": { "line": 2, "character": 0 }
        });

        // Delete spanning multiple lines
        LspProxy::apply_incremental_change(&mut text, &range, "").unwrap();
        assert_eq!(text, "line1line3\n");
    }

    #[test]
    fn test_position_to_offset_surrogate_pair() {
        // Text containing surrogate pair (emoji)
        // 😀 is U+1F600, 2 code units in UTF-16 (surrogate pair)
        // 4 bytes in UTF-8
        let text = "a😀b\n";

        // line 0, char 0 -> offset 0 (before 'a')
        assert_eq!(LspProxy::position_to_offset(text, 0, 0).unwrap(), 0);

        // line 0, char 1 -> offset 1 (before '😀')
        assert_eq!(LspProxy::position_to_offset(text, 0, 1).unwrap(), 1);

        // line 0, char 3 -> offset 5 (before 'b', emoji is 2 UTF-16 code units)
        assert_eq!(LspProxy::position_to_offset(text, 0, 3).unwrap(), 5);

        // line 0, char 4 -> offset 6 (before '\n')
        assert_eq!(LspProxy::position_to_offset(text, 0, 4).unwrap(), 6);
    }

    #[test]
    fn test_position_to_offset_line_end_clamp() {
        // Character exceeding line end is clamped to line end
        let text = "abc\ndef\n";

        // line 0, char 100 -> offset 3 (clamped to line end)
        assert_eq!(LspProxy::position_to_offset(text, 0, 100).unwrap(), 3);

        // line 1, char 100 -> offset 7 (clamped to line end)
        assert_eq!(LspProxy::position_to_offset(text, 1, 100).unwrap(), 7);
    }

    #[test]
    fn test_position_to_offset_line_out_of_range() {
        let text = "abc\ndef\n";

        // line 10 is out of range
        let result = LspProxy::position_to_offset(text, 10, 0);
        assert!(result.is_err());
    }

    #[test]
    fn test_apply_incremental_change_invalid_range() {
        // Invalid range where start > end
        let mut text = "hello world".to_string();
        let range = json!({
            "start": { "line": 0, "character": 10 },
            "end": { "line": 0, "character": 5 }
        });

        let result = LspProxy::apply_incremental_change(&mut text, &range, "test");
        assert!(result.is_err());
    }

    #[test]
    fn test_apply_incremental_change_with_emoji() {
        // Editing text containing emoji
        let mut text = "hello 😀 world".to_string();
        // Delete "😀 " (position 6 to 9: 😀 is 2 UTF-16 code units + 1 space)
        let range = json!({
            "start": { "line": 0, "character": 6 },
            "end": { "line": 0, "character": 9 }
        });

        LspProxy::apply_incremental_change(&mut text, &range, "").unwrap();
        assert_eq!(text, "hello world");
    }

    #[test]
    fn test_position_to_offset_empty_text() {
        let text = "";

        // line 0, char 0 is valid even for empty text
        assert_eq!(LspProxy::position_to_offset(text, 0, 0).unwrap(), 0);
    }

    #[test]
    fn test_position_to_offset_no_trailing_newline() {
        // Text without trailing newline
        let text = "abc";

        assert_eq!(LspProxy::position_to_offset(text, 0, 0).unwrap(), 0);
        assert_eq!(LspProxy::position_to_offset(text, 0, 3).unwrap(), 3);
    }
}
