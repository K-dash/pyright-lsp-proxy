use crate::backend_pool::shutdown_backend_instance;
use crate::error::ProxyError;
use crate::framing::LspFrameWriter;
use crate::message::{RpcId, RpcMessage};
use crate::venv;
use std::path::{Path, PathBuf};

impl super::LspProxy {
    /// Ensure a backend for the given URI's venv is in the pool.
    /// Returns Some(venv_path) if a backend is available, None if no venv found.
    pub(crate) async fn ensure_backend_in_pool(
        &mut self,
        url: &url::Url,
        file_path: &Path,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<Option<PathBuf>, ProxyError> {
        // Get venv from cache
        let target_venv = if let Some(doc) = self.state.open_documents.get(url) {
            doc.venv.clone()
        } else {
            tracing::debug!(uri = %url, "URI not in cache, searching venv");
            venv::find_venv(file_path, self.state.git_toplevel.as_deref()).await?
        };

        let target_venv = match target_venv {
            Some(v) => v,
            None => return Ok(None),
        };

        // Already in pool?
        if self.state.pool.contains(&target_venv) {
            return Ok(Some(target_venv));
        }

        // Need to create a new backend. Evict if full.
        if self.state.pool.is_full() {
            self.evict_lru_backend(client_writer).await?;
        }

        // Create backend instance
        let instance = self
            .create_backend_instance(&target_venv, client_writer)
            .await?;
        self.state.pool.insert(target_venv.clone(), instance);

        Ok(Some(target_venv))
    }

    /// Evict the LRU backend from the pool
    pub(crate) async fn evict_lru_backend(
        &mut self,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<(), ProxyError> {
        let pending_requests = &self.state.pending_requests;
        let lru_venv = self.state.pool.lru_venv(|venv, session| {
            pending_requests
                .values()
                .filter(|p| p.venv_path == *venv && p.backend_session == session)
                .count()
        });

        if let Some(venv_to_evict) = lru_venv {
            tracing::info!(
                venv = %venv_to_evict.display(),
                pool_size = self.state.pool.len(),
                "Evicting LRU backend"
            );

            if let Some(instance) = self.state.pool.remove(&venv_to_evict) {
                let evict_session = instance.session;

                // Cancel pending requests for this backend
                self.cancel_pending_requests_for_backend(
                    client_writer,
                    &venv_to_evict,
                    evict_session,
                )
                .await?;

                // Clean up pending_backend_requests for this backend
                self.clean_pending_backend_requests(&venv_to_evict, evict_session);

                // Clear diagnostics for documents under this venv
                self.clear_diagnostics_for_venv(&venv_to_evict, client_writer)
                    .await;

                // Shutdown
                shutdown_backend_instance(instance);
            }
        }

        Ok(())
    }

    /// Evict all expired backends (TTL-based auto-eviction).
    /// Skips backends that have pending client→backend or backend→client requests.
    pub(crate) async fn evict_expired_backends(
        &mut self,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<(), ProxyError> {
        let expired = self.state.pool.expired_venvs();
        if expired.is_empty() {
            return Ok(());
        }

        for venv_path in expired {
            let session = match self.state.pool.get(&venv_path) {
                Some(inst) => inst.session,
                None => continue,
            };

            // Skip if there are pending client→backend requests
            let pending_count = self
                .state
                .pending_requests
                .values()
                .filter(|p| p.venv_path == venv_path && p.backend_session == session)
                .count();
            if pending_count > 0 {
                tracing::debug!(
                    venv = %venv_path.display(),
                    pending_count = pending_count,
                    "Skipping TTL eviction: has pending client requests"
                );
                continue;
            }

            // Skip if there are pending backend→client requests
            let pending_backend_count = self
                .state
                .pending_backend_requests
                .values()
                .filter(|p| p.venv_path == venv_path && p.session == session)
                .count();
            if pending_backend_count > 0 {
                tracing::debug!(
                    venv = %venv_path.display(),
                    pending_backend_count = pending_backend_count,
                    "Skipping TTL eviction: has pending backend requests"
                );
                continue;
            }

            tracing::info!(
                venv = %venv_path.display(),
                pool_size = self.state.pool.len(),
                "Evicting expired backend (TTL)"
            );

            if let Some(instance) = self.state.pool.remove(&venv_path) {
                let evict_session = instance.session;

                self.cancel_pending_requests_for_backend(client_writer, &venv_path, evict_session)
                    .await?;

                self.clean_pending_backend_requests(&venv_path, evict_session);

                self.clear_diagnostics_for_venv(&venv_path, client_writer)
                    .await;

                shutdown_backend_instance(instance);
            }
        }

        Ok(())
    }

    /// Handle backend crash: remove from pool, cancel pending, clean up
    pub(crate) async fn handle_backend_crash(
        &mut self,
        venv_path: &PathBuf,
        session: u64,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<(), ProxyError> {
        // Verify session matches (avoid double-crash handling)
        let should_remove = self
            .state
            .pool
            .get(venv_path)
            .is_some_and(|inst| inst.session == session);

        if !should_remove {
            tracing::debug!(
                venv = %venv_path.display(),
                session = session,
                "Ignoring crash for already-removed backend"
            );
            return Ok(());
        }

        tracing::warn!(
            venv = %venv_path.display(),
            session = session,
            "Handling backend crash"
        );

        if let Some(instance) = self.state.pool.remove(venv_path) {
            // Cancel pending requests
            self.cancel_pending_requests_for_backend(client_writer, venv_path, session)
                .await?;

            // Clean up pending_backend_requests
            self.clean_pending_backend_requests(venv_path, session);

            // Abort reader task (it already exited with error, but be safe)
            instance.reader_task.abort();

            // Don't attempt graceful shutdown — process is already dead
            tracing::info!(
                venv = %venv_path.display(),
                session = session,
                "Backend removed from pool after crash"
            );
        }

        Ok(())
    }

    /// Cancel pending requests for a specific backend (identified by venv_path + session)
    pub(crate) async fn cancel_pending_requests_for_backend(
        &mut self,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
        venv_path: &PathBuf,
        session: u64,
    ) -> Result<(), ProxyError> {
        const REQUEST_CANCELLED: i64 = -32800;

        let to_cancel: Vec<RpcId> = self
            .state
            .pending_requests
            .iter()
            .filter(|(_, p)| p.venv_path == *venv_path && p.backend_session == session)
            .map(|(id, _)| id.clone())
            .collect();

        for id in to_cancel {
            self.state.pending_requests.remove(&id);
            let msg = RpcMessage {
                jsonrpc: "2.0".to_string(),
                id: Some(id.clone()),
                method: None,
                params: None,
                result: None,
                error: Some(crate::message::RpcError {
                    code: REQUEST_CANCELLED,
                    message: "Request cancelled due to backend eviction".to_string(),
                    data: None,
                }),
            };
            client_writer.write_message(&msg).await?;
            tracing::info!(id = ?id, venv = %venv_path.display(), session = session, "Cancelled pending request");
        }

        Ok(())
    }

    /// Clean up pending_backend_requests entries for a specific backend
    pub(crate) fn clean_pending_backend_requests(&mut self, venv_path: &PathBuf, session: u64) {
        self.state
            .pending_backend_requests
            .retain(|_, pending| !(pending.venv_path == *venv_path && pending.session == session));
    }
}
