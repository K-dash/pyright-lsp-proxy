use crate::backend_pool::{shutdown_backend_instance, BackendInstance};
use crate::error::ProxyError;
use crate::framing::LspFrameWriter;
use crate::message::{RpcId, RpcMessage};
use crate::venv;
use std::path::{Path, PathBuf};
use tokio::time::Instant;

/// Outcome of a pooled-backend venv identity check
/// (`check_pooled_venv_staleness`). See ARCHITECTURE.md's
/// "Venv Identity Tracking" section for the full design.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StaleOutcome {
    /// Venv identity unchanged (or the check was skipped/debounced).
    Fresh,
    /// The venv was replaced; the old backend was evicted and a new one spawned.
    Respawned,
    /// The venv was removed (missing beyond grace); the old backend was
    /// evicted with no respawn.
    Removed,
}

/// Result of inspecting a pooled backend's current venv identity, computed
/// with the pool borrow released so eviction/respawn can follow.
enum StaleDecision {
    Fresh,
    StillMissing,
    Mismatch,
    Removed,
}

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

        // Already in pool? Verify its venv identity hasn't gone stale before reusing it.
        if self.state.pool.contains(&target_venv) {
            return match self
                .check_pooled_venv_staleness(&target_venv, client_writer)
                .await?
            {
                // Respawned: the new backend is in the pool under the same
                // key; the caller still forwards its request to it. Restoration
                // did not replay this specific request.
                StaleOutcome::Fresh | StaleOutcome::Respawned => Ok(Some(target_venv)),
                // Removed: no backend left for this venv; caller falls back
                // to strict-mode ".venv not found" handling.
                StaleOutcome::Removed => Ok(None),
            };
        }

        // Need to create a new backend.
        self.spawn_and_insert_backend(&target_venv, client_writer)
            .await?;

        Ok(Some(target_venv))
    }

    /// Evict-if-full, spawn a new backend for `venv`, insert it into the pool,
    /// and warn if its project root is gitignored. Shared by pool-miss
    /// resolution and venv-replacement respawn.
    async fn spawn_and_insert_backend(
        &mut self,
        venv: &Path,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<(), ProxyError> {
        if self.state.pool.is_full() {
            self.evict_lru_backend(client_writer).await?;
        }

        let instance = self.create_backend_instance(venv, client_writer).await?;
        self.state.pool.insert(venv.to_path_buf(), instance);
        self.warn_if_project_root_ignored(venv, client_writer).await;

        Ok(())
    }

    /// Debounced check for whether a pooled backend's venv identity (derived
    /// from `.venv/pyvenv.cfg` metadata) has changed since it was last
    /// observed. Debounced to once per `venv_check_interval()`; disabled
    /// entirely when the interval is `0`.
    ///
    /// - Token mismatch: the venv was replaced. Evicts the old backend
    ///   (cancelling pending requests, clearing diagnostics, shutting down)
    ///   and respawns a new one via the same path pool-miss resolution uses.
    /// - `pyvenv.cfg` missing: served as-is until `2 * venv_check_interval()`
    ///   (grace) elapses, then evicted with no respawn; affected open
    ///   documents' cached venv is reset so the next request re-resolves it.
    pub(crate) async fn check_pooled_venv_staleness(
        &mut self,
        venv: &Path,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<StaleOutcome, ProxyError> {
        let interval = crate::backend_pool::venv_check_interval();
        if interval.is_zero() {
            return Ok(StaleOutcome::Fresh);
        }

        let venv_path = venv.to_path_buf();
        let now = Instant::now();
        let due = self
            .state
            .pool
            .get(&venv_path)
            .is_some_and(|inst| now.duration_since(inst.last_venv_check) >= interval);
        if !due {
            return Ok(StaleOutcome::Fresh);
        }

        // Stat before taking the pool borrow: `venv_token` is async and the
        // borrow must not be held across an `.await`.
        let current_token = venv::venv_token(venv).await;
        let grace = interval * 2;

        let decision = {
            let Some(inst) = self.state.pool.get_mut(&venv_path) else {
                // Backend disappeared mid-check (race with another eviction path).
                return Ok(StaleOutcome::Fresh);
            };
            inst.last_venv_check = Instant::now();

            if let Some(token) = current_token {
                inst.venv_missing_since = None;
                if inst.venv_token.is_some_and(|old| old != token) {
                    StaleDecision::Mismatch
                } else {
                    // Either it matches, or this is the first successful
                    // stat after a spawn-time capture race — adopt it.
                    inst.venv_token = Some(token);
                    StaleDecision::Fresh
                }
            } else {
                let missing_since = *inst.venv_missing_since.get_or_insert(now);
                if now.duration_since(missing_since) >= grace {
                    StaleDecision::Removed
                } else {
                    StaleDecision::StillMissing
                }
            }
        };

        match decision {
            StaleDecision::Fresh | StaleDecision::StillMissing => Ok(StaleOutcome::Fresh),
            StaleDecision::Mismatch => {
                self.evict_pooled_backend(&venv_path, client_writer).await?;
                self.notify_venv_replaced(&venv_path, client_writer).await;
                self.spawn_and_insert_backend(&venv_path, client_writer)
                    .await?;
                Ok(StaleOutcome::Respawned)
            }
            StaleDecision::Removed => {
                // Clear diagnostics (filtered by `doc.venv == venv_path`)
                // BEFORE resetting `doc.venv` below — ordering is load-bearing.
                self.evict_pooled_backend(&venv_path, client_writer).await?;
                self.reset_doc_venv_for(&venv_path);
                Ok(StaleOutcome::Removed)
            }
        }
    }

    /// Remove and clean up the pooled backend for `venv_path`, if present.
    async fn evict_pooled_backend(
        &mut self,
        venv_path: &PathBuf,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<(), ProxyError> {
        if let Some(instance) = self.state.pool.remove(venv_path) {
            let session = instance.session;
            self.cleanup_evicted_backend(instance, venv_path, session, client_writer, true)
                .await?;
        }
        Ok(())
    }

    /// Reset the cached venv of all open documents pointing at `venv_path`
    /// to `None`, so the next request re-resolves it via `find_venv`.
    fn reset_doc_venv_for(&mut self, venv_path: &Path) {
        for doc in self.state.open_documents.values_mut() {
            if doc.venv.as_deref() == Some(venv_path) {
                doc.venv = None;
            }
        }
    }

    /// TTL-sweep-adjacent staleness pass: evict (without respawning) any
    /// pooled backend whose venv was replaced or has been missing beyond
    /// grace. Log-only — no `window/showMessage`, no restoration. The next
    /// request lazily respawns via `ensure_backend_in_pool`. Skipped when
    /// venv identity tracking is disabled (`venv_check_interval() == 0`).
    pub(crate) async fn sweep_stale_venvs(
        &mut self,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<(), ProxyError> {
        let interval = crate::backend_pool::venv_check_interval();
        if interval.is_zero() {
            return Ok(());
        }
        let grace = interval * 2;
        let now = Instant::now();

        for venv_path in self.state.pool.backends_keys() {
            let current_token = venv::venv_token(&venv_path).await;

            let should_evict = {
                let Some(inst) = self.state.pool.get_mut(&venv_path) else {
                    continue;
                };
                if let Some(token) = current_token {
                    let mismatch = inst.venv_token.is_some_and(|old| old != token);
                    inst.venv_missing_since = None;
                    mismatch
                } else {
                    let missing_since = *inst.venv_missing_since.get_or_insert(now);
                    now.duration_since(missing_since) >= grace
                }
            };

            if !should_evict {
                continue;
            }

            tracing::info!(
                venv = %venv_path.display(),
                reason = if current_token.is_some() { "replaced" } else { "removed" },
                "Sweep: evicting stale pooled backend"
            );

            self.evict_pooled_backend(&venv_path, client_writer).await?;
            if current_token.is_none() {
                self.reset_doc_venv_for(&venv_path);
            }
        }

        Ok(())
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
