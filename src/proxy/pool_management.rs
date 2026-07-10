use crate::backend_pool::{shutdown_backend_instance, BackendInstance};
use crate::error::ProxyError;
use crate::framing::LspFrameWriter;
use crate::message::{RpcId, RpcMessage};
use crate::venv;
use std::path::{Path, PathBuf};

impl super::LspProxy {
    /// Ensure a backend for the given URI's venv is in the pool.
    /// Returns `Some(venv_path)` if a backend is available, None if no venv found.
    pub(crate) async fn ensure_backend_in_pool(
        &mut self,
        url: &url::Url,
        file_path: &Path,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<Option<PathBuf>, ProxyError> {
        // Get venv from cache (clone to avoid borrow conflict with later get_mut)
        let cached_venv = self
            .state
            .open_documents
            .get(url)
            .map(|doc| doc.venv.clone());

        let target_venv = match cached_venv {
            Some(Some(v)) => Some(v),
            Some(None) => {
                // venv was not found when the document was opened.
                // Re-search in case .venv was created after didOpen.
                let found = venv::find_venv(file_path, self.state.git_toplevel.as_deref());
                if let Some(ref venv_path) = found {
                    if let Some(doc) = self.state.open_documents.get_mut(url) {
                        doc.venv = Some(venv_path.clone());
                    }
                    tracing::info!(uri = %url, venv = %venv_path.display(), "venv discovered after didOpen, cache updated");
                }
                found
            }
            None => {
                tracing::debug!(uri = %url, "URI not in cache, searching venv");
                venv::find_venv(file_path, self.state.git_toplevel.as_deref())
            }
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
        self.warn_if_project_root_ignored(&target_venv, client_writer)
            .await;

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
                self.cleanup_evicted_backend(
                    instance,
                    &venv_to_evict,
                    evict_session,
                    client_writer,
                    true,
                )
                .await?;
            }
        }

        Ok(())
    }

    /// Evict all expired backends (TTL-based auto-eviction).
    /// Skips backends that have pending client→backend or backend→client requests.
    ///
    /// The removed instance holds a child-process handle with a custom `Drop`,
    /// so its drop order will change under Edition 2024. Deferred to the
    /// eventual edition migration rather than restructured here to satisfy
    /// the lint.
    #[allow(tail_expr_drop_order)]
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
                self.cleanup_evicted_backend(
                    instance,
                    &venv_path,
                    evict_session,
                    client_writer,
                    true,
                )
                .await?;
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
            // do_shutdown=false: process is already dead, just abort reader + clean up
            self.cleanup_evicted_backend(instance, venv_path, session, client_writer, false)
                .await?;

            tracing::info!(
                venv = %venv_path.display(),
                session = session,
                "Backend removed from pool after crash"
            );
        }

        Ok(())
    }

    /// Clean up after removing a backend instance from the pool.
    /// Cancels pending requests, clears diagnostics, and shuts down the process.
    /// Set `do_shutdown` to false for crashed backends (process already dead).
    async fn cleanup_evicted_backend(
        &mut self,
        instance: BackendInstance,
        venv_path: &PathBuf,
        session: u64,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
        do_shutdown: bool,
    ) -> Result<(), ProxyError> {
        self.cancel_pending_requests_for_backend(client_writer, venv_path, session)
            .await?;
        self.clean_pending_backend_requests(venv_path, session);
        self.clear_diagnostics_for_venv(venv_path, client_writer)
            .await;
        if do_shutdown {
            shutdown_backend_instance(instance);
        } else {
            instance.reader_task.abort();
        }
        Ok(())
    }

    /// Cancel pending requests for a specific backend (identified by `venv_path` + session).
    /// Also handles fan-out sub-requests: removes them from pending fanouts and
    /// completes any fanouts that have no remaining sub-requests.
    pub(crate) async fn cancel_pending_requests_for_backend(
        &mut self,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
        venv_path: &PathBuf,
        session: u64,
    ) -> Result<(), ProxyError> {
        // First: cancel fan-out sub-requests for this backend
        let affected_fanout_ids = self.cancel_fanout_sub_requests(venv_path, session);

        // Then: cancel normal pending requests (fan-out sub-requests are already removed)
        let to_cancel: Vec<RpcId> = self
            .state
            .pending_requests
            .iter()
            .filter(|(_, p)| p.venv_path == *venv_path && p.backend_session == session)
            .map(|(id, _)| id.clone())
            .collect();

        for id in to_cancel {
            self.state.pending_requests.remove(&id);
            let msg = RpcMessage::cancelled_response(
                id.clone(),
                "Request cancelled due to backend eviction",
            );
            client_writer.write_message(&msg).await?;
            tracing::info!(id = ?id, venv = %venv_path.display(), session = session, "Cancelled pending request");
        }

        // Complete any fan-outs that have no remaining sub-requests
        for client_id in affected_fanout_ids {
            if let Some(fanout) = self.state.pending_fanouts.remove(&client_id) {
                self.complete_fanout(fanout, client_writer).await?;
            }
        }

        Ok(())
    }

    /// Clean up `pending_backend_requests` entries for a specific backend
    pub(crate) fn clean_pending_backend_requests(&mut self, venv_path: &PathBuf, session: u64) {
        self.state
            .pending_backend_requests
            .retain(|_, pending| !(pending.venv_path == *venv_path && pending.session == session));
    }

    /// Transition all warming backends past their deadline to Ready (fail-open).
    pub(crate) async fn expire_warmup_backends(
        &mut self,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<(), ProxyError> {
        let expired: Vec<PathBuf> = self
            .state
            .pool
            .warming_backends()
            .into_iter()
            .filter(|venv| {
                self.state
                    .pool
                    .get(venv)
                    .is_some_and(super::super::backend_pool::BackendInstance::warmup_expired)
            })
            .collect();

        for venv_path in expired {
            let session = match self.state.pool.get(&venv_path) {
                Some(inst) if inst.is_warming() => inst.session,
                _ => continue,
            };

            if let Some(inst) = self.state.pool.get_mut(&venv_path) {
                tracing::info!(
                    venv = %venv_path.display(),
                    "Backend warmup complete (reason: timeout), transitioning to Ready (fail-open)"
                );
                let queued = inst.mark_ready();
                if !queued.is_empty() {
                    self.drain_warmup_queue(&venv_path, session, queued, client_writer)
                        .await?;
                }
            }
        }

        Ok(())
    }
}
