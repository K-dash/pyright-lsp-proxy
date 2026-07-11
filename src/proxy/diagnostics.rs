use crate::backend_pool::MAX_CREATING_QUEUE_LEN;
use crate::error::ProxyError;
use crate::framing::LspFrameWriter;
use crate::message::RpcMessage;
use crate::venv;
use std::path::Path;
use tokio::time::Instant;

impl super::LspProxy {
    /// Warn the client if a venv's project root is gitignored from the proxy's cwd.
    ///
    /// Claude Code's LSP tool filters goToDefinition/findReferences/etc. results
    /// through `git check-ignore` run in the session cwd, silently dropping
    /// locations under gitignored paths (anthropics/claude-code#76371). Checked
    /// at most once per venv per proxy lifetime.
    pub(crate) async fn warn_if_project_root_ignored(
        &mut self,
        venv_path: &Path,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) {
        if self.state.ignore_checked_venvs.contains(venv_path) {
            return;
        }
        self.state
            .ignore_checked_venvs
            .insert(venv_path.to_path_buf());

        let Some(project_root) = venv_path.parent() else {
            return;
        };

        let cwd = match std::env::current_dir() {
            Ok(cwd) => cwd,
            Err(e) => {
                tracing::warn!(error = ?e, "Failed to get current directory for gitignore check");
                return;
            }
        };

        if !venv::is_path_git_ignored(project_root, &cwd).await {
            return;
        }

        tracing::warn!(
            venv = %venv_path.display(),
            project_root = %project_root.display(),
            "Project root is gitignored from session cwd; Claude Code will silently hide goToDefinition/findReferences results"
        );

        let msg = RpcMessage::notification(
            "window/showMessage",
            Some(serde_json::json!({
                "type": 2,
                "message": format!(
                    "typemux-cc: project root {} is gitignored from the session working directory. Claude Code silently hides goToDefinition/findReferences results for gitignored paths (anthropics/claude-code#76371). Launch Claude Code inside this directory to avoid this.",
                    project_root.display()
                )
            })),
        );

        if let Err(e) = client_writer.write_message(&msg).await {
            tracing::warn!(
                error = ?e,
                "Failed to send gitignored project root warning to client"
            );
        }
    }

    /// Send window/showMessage error to client when backend creation fails.
    ///
    /// Logs every failure server-side. The client-visible notification is
    /// deduplicated per venv within a TTL (see
    /// `ProxyState::should_notify_backend_failure`) so that repeated
    /// failures (e.g. `didOpen` for several files under the same broken
    /// venv) don't spam `window/showMessage` (issue #26).
    pub(crate) async fn notify_backend_error(
        &mut self,
        venv_path: &Path,
        error: &ProxyError,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) {
        tracing::error!(
            venv = %venv_path.display(),
            error = ?error,
            "Backend creation failed"
        );

        if !self
            .state
            .should_notify_backend_failure(venv_path, Instant::now())
        {
            return;
        }

        let msg = RpcMessage::notification(
            "window/showMessage",
            Some(serde_json::json!({
                "type": 1,
                "message": format!(
                    "typemux-cc: Failed to start LSP backend for {}: {}",
                    venv_path.display(),
                    error
                )
            })),
        );

        if let Err(e) = client_writer.write_message(&msg).await {
            tracing::warn!(
                error = ?e,
                "Failed to send backend error notification to client"
            );
        }
    }

    /// Send a dedup'd `window/showMessage` warning when a Creating venv's
    /// queue overflows and a notification (no response to send an error for)
    /// was dropped instead of queued. Shares `should_notify_backend_failure`'s
    /// per-venv dedup TTL and map with backend-creation-failure notifications
    /// — both report a problem with the same venv, so sharing the dedup
    /// budget avoids doubling up on `window/showMessage` spam.
    pub(crate) async fn notify_creating_queue_overflow(
        &mut self,
        venv_path: &Path,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) {
        if !self
            .state
            .should_notify_backend_failure(venv_path, Instant::now())
        {
            return;
        }

        let msg = RpcMessage::notification(
            "window/showMessage",
            Some(serde_json::json!({
                "type": 2,
                "message": format!(
                    "typemux-cc: {} is still starting and its request queue is full ({MAX_CREATING_QUEUE_LEN} pending); a notification was dropped.",
                    venv_path.display()
                )
            })),
        );

        if let Err(e) = client_writer.write_message(&msg).await {
            tracing::warn!(
                error = ?e,
                "Failed to send creating-queue-overflow warning to client"
            );
        }
    }

    /// Notify the client that a pooled backend's venv was replaced (e.g. by
    /// `uv sync` recreating `.venv`) and the backend is being restarted.
    pub(crate) async fn notify_venv_replaced(
        &self,
        venv_path: &Path,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) {
        let msg = RpcMessage::notification(
            "window/showMessage",
            Some(serde_json::json!({
                "type": 3,
                "message": format!(
                    "typemux-cc: {} was replaced with a new environment; restarting the LSP backend.",
                    venv_path.display()
                )
            })),
        );

        if let Err(e) = client_writer.write_message(&msg).await {
            tracing::warn!(
                error = ?e,
                "Failed to send venv-replaced notification to client"
            );
        }
    }

    /// Clear diagnostics for all documents belonging to a venv
    pub(crate) async fn clear_diagnostics_for_venv(
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

    /// Clear diagnostics for specified URIs (send empty array)
    pub(crate) async fn clear_diagnostics_for_uris(
        &self,
        uris: &[url::Url],
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> (usize, usize) {
        let mut ok = 0;
        let mut failed = 0;

        for uri in uris {
            tracing::trace!(uri = %uri, "Clearing diagnostics");

            let clear_msg = RpcMessage::notification(
                "textDocument/publishDiagnostics",
                Some(serde_json::json!({
                    "uri": uri.to_string(),
                    "diagnostics": []
                })),
            );

            match client_writer.write_message(&clear_msg).await {
                Ok(()) => ok += 1,
                Err(e) => {
                    failed += 1;
                    tracing::warn!(uri = %uri, error = ?e, "Failed to clear diagnostics");
                }
            }
        }

        (ok, failed)
    }
}
