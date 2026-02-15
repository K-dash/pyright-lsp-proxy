use crate::error::ProxyError;
use crate::framing::LspFrameWriter;
use crate::message::RpcMessage;
use std::path::Path;

impl super::LspProxy {
    /// Send window/showMessage error to client when backend creation fails
    pub(crate) async fn notify_backend_error(
        &self,
        venv_path: &Path,
        error: &ProxyError,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) {
        let msg = RpcMessage {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: Some("window/showMessage".to_string()),
            params: Some(serde_json::json!({
                "type": 1,
                "message": format!(
                    "typemux-cc: Failed to start LSP backend for {}: {}",
                    venv_path.display(),
                    error
                )
            })),
            result: None,
            error: None,
        };

        if let Err(e) = client_writer.write_message(&msg).await {
            tracing::warn!(
                error = ?e,
                "Failed to send backend error notification to client"
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
}
