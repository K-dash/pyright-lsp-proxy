use crate::error::ProxyError;
use crate::framing::LspFrameWriter;
use crate::message::RpcMessage;
use crate::venv;
use std::path::PathBuf;
use tokio::time::Instant;

impl super::LspProxy {
    /// Extract textDocument.uri from LSP request params
    pub(crate) fn extract_text_document_uri(msg: &RpcMessage) -> Option<url::Url> {
        let params = msg.params.as_ref()?;
        let text_document = params.get("textDocument")?;
        let uri_str = text_document.get("uri")?.as_str()?;
        url::Url::parse(uri_str).ok()
    }

    /// Get the venv path for a document URI from cache
    pub(crate) fn venv_for_uri(&self, url: &url::Url) -> Option<PathBuf> {
        self.state
            .open_documents
            .get(url)
            .and_then(|doc| doc.venv.clone())
    }

    /// Handle didOpen: cache document, ensure backend in pool, forward
    pub(crate) async fn handle_did_open(
        &mut self,
        msg: &RpcMessage,
        count: usize,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<(), ProxyError> {
        let Some(params) = &msg.params else {
            return Ok(());
        };
        let Some(text_document) = params.get("textDocument") else {
            return Ok(());
        };

        let text = text_document
            .get("text")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string());

        let Some(uri_str) = text_document.get("uri").and_then(|u| u.as_str()) else {
            return Ok(());
        };
        let Ok(url) = url::Url::parse(uri_str) else {
            return Ok(());
        };
        let Ok(file_path) = url.to_file_path() else {
            return Ok(());
        };

        let language_id = text_document
            .get("languageId")
            .and_then(|l| l.as_str())
            .unwrap_or("unknown")
            .to_string();

        let version = text_document
            .get("version")
            .and_then(|v| v.as_i64())
            .unwrap_or(0) as i32;

        tracing::info!(
            count = count,
            uri = uri_str,
            path = %file_path.display(),
            "didOpen received"
        );

        // Search for .venv
        let found_venv = venv::find_venv(&file_path, self.state.git_toplevel.as_deref()).await?;

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
        let Some(ref venv_path) = found_venv else {
            tracing::debug!(
                uri = uri_str,
                "No venv found for document, not forwarding didOpen"
            );
            return Ok(());
        };

        if !self.state.pool.contains(venv_path) {
            // Need to create backend
            if self.state.pool.is_full() {
                self.evict_lru_backend(client_writer).await?;
            }

            match self.create_backend_instance(venv_path, client_writer).await {
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

        Ok(())
    }

    /// Handle didChange
    pub(crate) async fn handle_did_change(&mut self, msg: &RpcMessage) -> Result<(), ProxyError> {
        let Some(params) = &msg.params else {
            return Ok(());
        };
        let Some(text_document) = params.get("textDocument") else {
            return Ok(());
        };
        let Some(uri_str) = text_document.get("uri").and_then(|u| u.as_str()) else {
            return Ok(());
        };
        let Ok(url) = url::Url::parse(uri_str) else {
            return Ok(());
        };

        let version = text_document
            .get("version")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32);

        let Some(content_changes) = params.get("contentChanges") else {
            return Ok(());
        };
        let Some(changes_array) = content_changes.as_array() else {
            return Ok(());
        };

        if changes_array.is_empty() {
            tracing::debug!(
                uri = %url,
                "didChange received with empty contentChanges, ignoring"
            );
            return Ok(());
        }

        let Some(doc) = self.state.open_documents.get_mut(&url) else {
            tracing::warn!(
                uri = %url,
                "didChange for unopened document, ignoring"
            );
            return Ok(());
        };

        for change in changes_array {
            if let Some(range) = change.get("range") {
                if let Some(new_text) = change.get("text").and_then(|t| t.as_str()) {
                    crate::text_edit::apply_incremental_change(&mut doc.text, range, new_text)?;
                }
            } else if let Some(new_text) = change.get("text").and_then(|t| t.as_str()) {
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

        Ok(())
    }

    /// Handle didClose: remove document from cache
    pub(crate) async fn handle_did_close(&mut self, msg: &RpcMessage) -> Result<(), ProxyError> {
        let Some(url) = Self::extract_text_document_uri(msg) else {
            return Ok(());
        };

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

        Ok(())
    }
}
