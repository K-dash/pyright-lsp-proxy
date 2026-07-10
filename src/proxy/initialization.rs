use crate::backend::LspBackend;
use crate::backend_pool::BackendInstance;
use crate::error::ProxyError;
use crate::framing::LspFrameWriter;
use crate::message::{RpcId, RpcMessage};
use serde_json::Value;
use std::path::Path;
use url::Url;

/// Rewrite rootUri, rootPath, and workspaceFolders in initialize params
/// to point to the venv's parent directory (the project root).
///
/// This ensures each backend indexes only the project that owns the venv,
/// which is critical for worktree paths (dot-prefixed directories like
/// `.worktree/` are excluded from indexing when rootUri points to the
/// main repo root).
fn rewrite_root_uri(init_params: &mut Value, venv: &Path) {
    let project_root = match venv.parent() {
        Some(p) => p,
        None => return,
    };

    let root_uri = match Url::from_file_path(project_root) {
        Ok(u) => u.to_string(),
        Err(()) => return,
    };

    let dir_name = project_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace");

    let root_path = project_root.to_string_lossy().to_string();

    tracing::info!(
        root_uri = %root_uri,
        root_path = %root_path,
        "Rewriting initialize params rootUri to venv project root"
    );

    if let Some(obj) = init_params.as_object_mut() {
        obj.insert("rootUri".to_string(), Value::String(root_uri.clone()));
        obj.insert("rootPath".to_string(), Value::String(root_path));
        obj.insert(
            "workspaceFolders".to_string(),
            serde_json::json!([{"uri": root_uri, "name": dir_name}]),
        );
    }
}

/// Perform the LSP initialize handshake with a backend:
/// 1. Send `initialize` request with the given params
/// 2. Wait for the initialize response (10s timeout, skip notifications)
/// 3. Send `initialized` notification
///
/// Returns the initialize response from the backend.
async fn perform_initialize_handshake(
    backend: &mut LspBackend,
    mut init_params: Value,
    venv: &Path,
) -> Result<RpcMessage, ProxyError> {
    rewrite_root_uri(&mut init_params, venv);
    tracing::trace!(
        venv = %venv.display(),
        init_params = %init_params,
        "Full initialize params sent to backend"
    );
    let init_msg = RpcMessage::request(RpcId::Number(1), "initialize", Some(init_params));

    tracing::info!(venv = %venv.display(), "Sending initialize to backend");
    backend.send_message(&init_msg).await?;

    // Receive initialize response
    let init_id = 1i64;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let init_response = loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(ProxyError::Backend(
                crate::error::BackendError::InitializeTimeout(10),
            ));
        }

        let wait_result = tokio::time::timeout(remaining, backend.read_message()).await;

        match wait_result {
            Ok(Ok(msg)) => {
                if msg.is_response() {
                    if let Some(RpcId::Number(id)) = &msg.id {
                        if *id == init_id {
                            if let Some(error) = &msg.error {
                                return Err(ProxyError::Backend(
                                    crate::error::BackendError::InitializeResponseError(format!(
                                        "code={}, message={}",
                                        error.code, error.message
                                    )),
                                ));
                            }
                            tracing::info!(
                                venv = %venv.display(),
                                "Received initialize response from backend"
                            );
                            break msg;
                        }
                    }
                } else {
                    tracing::debug!(
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
    };

    // Send initialized notification
    let initialized_msg = RpcMessage::notification("initialized", Some(serde_json::json!({})));

    tracing::info!(venv = %venv.display(), "Sending initialized to backend");
    backend.send_message(&initialized_msg).await?;

    Ok(init_response)
}

impl super::LspProxy {
    /// Extract cached initialize params, returning an error if not available.
    fn cached_init_params(&self) -> Result<Value, ProxyError> {
        self.state
            .client_initialize
            .as_ref()
            .and_then(|msg| msg.params.clone())
            .ok_or_else(|| ProxyError::InvalidMessage("No initialize params cached".to_string()))
    }

    /// Complete backend initialization: forward initialize, receive response, send initialized.
    /// Returns the initialize response to forward to the client.
    pub(crate) async fn complete_backend_initialization(
        &self,
        backend: &mut LspBackend,
        venv: &Path,
        _client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<RpcMessage, ProxyError> {
        let init_params = self.cached_init_params()?;
        perform_initialize_handshake(backend, init_params, venv).await
    }

    /// Create a new backend, initialize it, split it, and return a `BackendInstance`.
    /// Does NOT insert into the pool — caller is responsible for that.
    pub(crate) async fn create_backend_instance(
        &mut self,
        venv: &Path,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<BackendInstance, ProxyError> {
        let session = self.state.pool.next_session_id();

        tracing::info!(
            session = session,
            venv = %venv.display(),
            "Creating new backend instance"
        );

        // 1. Spawn
        let mut backend = LspBackend::spawn(self.state.backend_kind, Some(venv))?;

        // 2. Initialize handshake
        let init_params = self.cached_init_params()?;
        perform_initialize_handshake(&mut backend, init_params, venv).await?;
        tracing::info!(session = session, venv = %venv.display(), "Backend initialized");

        // 3. Document restoration for this venv
        self.restore_documents_to_backend(&mut backend, venv, session, client_writer)
            .await?;

        // 4. Capture venv identity token (None if capture raced a rebuild)
        let venv_token = crate::venv::venv_token(venv).await;

        // 5. Split and create instance
        let parts = backend.into_split();
        let tx = self.state.pool.msg_sender();
        Ok(BackendInstance::from_parts(
            parts,
            venv.to_path_buf(),
            session,
            tx,
            venv_token,
        ))
    }

    /// Restore documents belonging to a venv to a backend
    pub(crate) async fn restore_documents_to_backend(
        &self,
        backend: &mut LspBackend,
        venv: &Path,
        session: u64,
        _client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<(), ProxyError> {
        let venv_parent = venv.parent().map(std::path::Path::to_path_buf);
        let total_docs = self.state.open_documents.len();
        let mut restored = 0;
        let mut skipped = 0;
        let mut failed = 0;

        tracing::info!(
            session = session,
            total_docs = total_docs,
            venv_parent = ?venv_parent.as_ref().map(|p| p.display().to_string()),
            "Starting document restoration"
        );

        for (url, doc) in &self.state.open_documents {
            // Only restore documents matching this venv
            let should_restore = doc.venv.as_deref() == Some(venv)
                || match (url.to_file_path().ok(), &venv_parent) {
                    (Some(file_path), Some(vp)) => file_path.starts_with(vp),
                    _ => false,
                };

            if !should_restore {
                skipped += 1;
                continue;
            }

            let uri_str = url.to_string();
            let language_id = doc.language_id.clone();
            let version = doc.version;
            let text = doc.text.clone();
            let text_len = text.len();

            let didopen_msg = RpcMessage::notification(
                "textDocument/didOpen",
                Some(serde_json::json!({
                    "textDocument": {
                        "uri": uri_str,
                        "languageId": language_id,
                        "version": version,
                        "text": text,
                    }
                })),
            );

            match backend.send_message(&didopen_msg).await {
                Ok(()) => {
                    restored += 1;
                    tracing::info!(
                        session = session,
                        uri = %uri_str,
                        text_len = text_len,
                        "Restored document"
                    );
                }
                Err(e) => {
                    failed += 1;
                    tracing::error!(
                        session = session,
                        uri = %uri_str,
                        error = ?e,
                        "Failed to restore document"
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

        Ok(())
    }
}
