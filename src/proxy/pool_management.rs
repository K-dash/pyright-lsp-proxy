use super::initialization::{should_restore_document, spawn_backend_creation_task};
use crate::backend_pool::{
    shutdown_backend_instance, BackendInstance, CreatingEntry, CreationOutcome,
};
use crate::error::{BackendError, ProxyError};
use crate::framing::LspFrameWriter;
use crate::message::{RpcId, RpcMessage};
use crate::state::OpenDocument;
use crate::venv;
use std::path::{Path, PathBuf};
use tokio::time::Instant;
use url::Url;

/// Outcome of a pooled-backend venv identity check
/// (`check_pooled_venv_staleness`). See ARCHITECTURE.md's
/// "Venv Identity Tracking" section for the full design.
#[derive(Debug)]
pub(crate) enum StaleOutcome {
    /// Venv identity unchanged (or the check was skipped/debounced).
    Fresh,
    /// The venv was replaced; the old backend was evicted and creation of a
    /// new one STARTED (the new instance is `Creating`, not yet `Ready`).
    Respawned,
    /// The venv was removed (missing beyond grace); the old backend was
    /// evicted with no respawn.
    Removed,
    /// The venv was replaced and the old backend evicted, but starting the
    /// replacement's creation failed synchronously (pool busy, or a
    /// client-writer error during eviction). The pool entry is gone, so the
    /// next request for this venv retries via the pool-miss path.
    RespawnFailed(ProxyError),
}

/// Result of inspecting a pooled backend's current venv identity, computed
/// with the pool borrow released so eviction/respawn can follow.
enum StaleDecision {
    Fresh,
    StillMissing,
    Mismatch,
    Removed,
}

/// Outcome of resolving a backend for a request/notification's venv.
/// See ARCHITECTURE.md's Creating-state section.
pub(crate) enum PoolLookup {
    /// A `Ready` backend is available; forward to it directly.
    Ready(PathBuf),
    /// This call started backend creation for the venv — for `didOpen`, the
    /// snapshot taken by `start_backend_creation` already captured this
    /// document, so no further queueing is needed.
    CreatingStarted(PathBuf),
    /// Backend creation for the venv was already in flight before this
    /// call — the current message is NOT covered by the snapshot already
    /// taken and must be queued into `CreatingEntry.queued`.
    CreatingInFlight(PathBuf),
    /// No venv resolved, or the venv was removed with nothing to serve it.
    NotFound,
}

impl super::LspProxy {
    /// Ensure a backend for the given URI's venv is in the pool (Ready) or at
    /// least being created (Creating). See `PoolLookup` for what each
    /// variant means to the caller.
    pub(crate) async fn ensure_backend_in_pool(
        &mut self,
        url: &url::Url,
        file_path: &Path,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<PoolLookup, ProxyError> {
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
            None => return Ok(PoolLookup::NotFound),
        };

        // Already in pool? Verify its venv identity hasn't gone stale before reusing it.
        if self.state.pool.contains(&target_venv) {
            return match self
                .check_pooled_venv_staleness(&target_venv, client_writer)
                .await?
            {
                StaleOutcome::Fresh => Ok(PoolLookup::Ready(target_venv)),
                // Respawned: staleness check itself started a fresh
                // creation for this venv (old backend already evicted).
                StaleOutcome::Respawned => Ok(PoolLookup::CreatingStarted(target_venv)),
                // Removed: no backend left for this venv; caller falls back
                // to strict-mode ".venv not found" handling.
                StaleOutcome::Removed => Ok(PoolLookup::NotFound),
                // RespawnFailed: surface the creation error through the
                // callers' existing containment (didOpen notifies, requests
                // get a JSON-RPC error response).
                StaleOutcome::RespawnFailed(e) => Err(e),
            };
        }

        // A creation for this venv may already be in flight (e.g. a
        // concurrent request for the same cold venv) — don't double-spawn.
        if self.state.pool.creating_contains(&target_venv) {
            return Ok(PoolLookup::CreatingInFlight(target_venv));
        }

        self.start_backend_creation(&target_venv, client_writer)
            .await?;

        Ok(PoolLookup::CreatingStarted(target_venv))
    }

    /// Evict-if-full, allocate a session, snapshot the documents to restore,
    /// and spawn a creation task for `venv` off the event loop. Returns once
    /// the task has been kicked off — NOT once it has completed. Shared by
    /// pool-miss resolution and venv-replacement respawn.
    ///
    /// Only the synchronous prefix can fail here: `PoolBusy` (no evictable
    /// Ready backend) or a client-writer error during eviction. Failures of
    /// the spawn/handshake/restoration itself surface later, asynchronously,
    /// via the `creation_rx` completion handler.
    async fn start_backend_creation(
        &mut self,
        venv: &Path,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<(), ProxyError> {
        if self.state.pool.is_full() {
            if self.state.pool.is_empty() {
                // Every slot is Creating and none is a Ready backend to
                // evict: no implicit waiting, fail loudly instead.
                return Err(ProxyError::PoolBusy(venv.to_path_buf()));
            }
            self.evict_lru_backend(client_writer).await?;
        }

        let session = self.state.pool.next_session_id();
        let venv_parent = venv.parent().map(Path::to_path_buf);
        let restore_snapshot: Vec<(Url, OpenDocument)> = self
            .state
            .open_documents
            .iter()
            .filter(|(url, doc)| {
                let file_path = url.to_file_path().ok();
                should_restore_document(
                    doc.venv.as_deref(),
                    venv,
                    file_path.as_deref(),
                    venv_parent.as_deref(),
                )
            })
            .map(|(url, doc)| (url.clone(), doc.clone()))
            .collect();
        let init_params = self.cached_init_params()?;

        self.state
            .pool
            .creating_insert(venv.to_path_buf(), CreatingEntry::new(session));

        tracing::info!(
            session = session,
            venv = %venv.display(),
            restore_count = restore_snapshot.len(),
            "Starting backend creation"
        );

        spawn_backend_creation_task(
            self.state.backend_kind,
            venv.to_path_buf(),
            session,
            init_params,
            restore_snapshot,
            self.state.pool.msg_sender(),
            self.state.pool.creation_sender(),
        );

        Ok(())
    }

    /// Handle a completed (or failed) backend-creation task, reported via
    /// `creation_rx`. Only this method removes an entry from
    /// `BackendPool::creating`.
    ///
    /// Success (and actually alive): insert the new `BackendInstance` into
    /// `backends`, then move the entry's queued messages into
    /// `ProxyState::replay_queue` — a plain `VecDeque::extend`, no I/O, no
    /// backend writes here (see ARCHITECTURE.md on why replay must not be a
    /// batch loop).
    ///
    /// Success but already dead, or failure: every queued *request* gets an
    /// immediate JSON-RPC error response (bounded to `MAX_CREATING_QUEUE_LEN`,
    /// so this is a small, finite loop, unlike replay); queued notifications
    /// are dropped, and a single dedup'd `window/showMessage` reports the
    /// failure (#92 containment: notify + lazy retry, never process exit).
    pub(crate) async fn handle_creation_outcome(
        &mut self,
        outcome: CreationOutcome,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<(), ProxyError> {
        let CreationOutcome { venv_path, result } = outcome;

        let Some(entry) = self.state.pool.creating_remove(&venv_path) else {
            // Defensive: no other code path removes `creating` entries.
            tracing::warn!(venv = %venv_path.display(), "Creation outcome for an untracked venv, ignoring");
            return Ok(());
        };

        let result = match result {
            // A restoration write failure already fails creation before this
            // point (see `write_restored_documents`), but a backend can also
            // die *without* any write failing — e.g. every write lands in
            // the OS pipe buffer moments before the process exits. Check
            // liveness before trusting `Ok`. This narrows but doesn't close
            // the race: the process could still die in the instant after
            // `try_wait`, same residual window a `Ready` backend already
            // lives with (caught on its next read/write instead).
            //
            // `try_wait` only detects the PROCESS dying. A backend can also
            // emit a malformed/oversized frame (#106) — or, pre-dating that
            // fix, a bare `MissingContentLength` from a backend that dies
            // mid-write — while staying alive: the reader task
            // (`spawn_reader_task`) hits the framing error and exits, but
            // `try_wait` still reports the process as running. Without this
            // second check, such an instance would be pooled `Ready` with no
            // task left to ever drain its stdout, and every subsequent
            // request against it would write into the pipe and hang forever
            // waiting for a response nobody reads. `is_finished()` catches
            // that: the reader task is gone, so this is a dead backend
            // regardless of what the OS process is doing.
            Ok(mut instance) => match instance.child.try_wait() {
                Ok(None) => {
                    if instance.reader_task.is_finished() {
                        instance.reader_task.abort();
                        Err(ProxyError::Backend(BackendError::InitializeFailed(
                            "backend reader task exited during creation (framing error) \
                             while the process is still alive"
                                .to_string(),
                        )))
                    } else {
                        Ok(instance)
                    }
                }
                Ok(Some(status)) => {
                    instance.reader_task.abort();
                    Err(ProxyError::Backend(BackendError::InitializeFailed(
                        format!("backend process exited during creation ({status})"),
                    )))
                }
                Err(io_err) => {
                    instance.reader_task.abort();
                    Err(ProxyError::Backend(BackendError::InitializeFailed(
                        format!("failed to check backend liveness after creation: {io_err}"),
                    )))
                }
            },
            Err(e) => Err(e),
        };

        match result {
            Ok(instance) => {
                tracing::info!(
                    venv = %venv_path.display(),
                    session = entry.session,
                    queued = entry.queued.len(),
                    "Backend creation completed"
                );
                self.state.pool.insert(venv_path.clone(), instance);
                self.state.venv_error_notified_at.remove(&venv_path);
                self.warn_if_project_root_ignored(&venv_path, client_writer)
                    .await;
                self.state.replay_queue.extend(entry.queued);
            }
            Err(e) => {
                tracing::error!(
                    venv = %venv_path.display(),
                    session = entry.session,
                    error = ?e,
                    "Backend creation failed"
                );
                // Requests get an immediate error response (no backend
                // will ever exist for them). Notifications are different:
                // round 2's Creating-state design defers their cache
                // mutation (didClose's removal, didChange's edit) to
                // whichever pass finds the venv no longer Creating, on the
                // assumption that pass is always a replay. Dropping them
                // here instead of replaying them would mean that mutation
                // never runs at all — a didClose's document stays open
                // forever (the ghost-document bug round 2 fixed, back via
                // the failure path instead of the success path), and a
                // didChange's edit is lost, so the next restoration replays
                // stale text. So notifications go to `replay_queue` here
                // too, exactly like the success arm above, even though
                // there's no backend for them to reach: on re-dispatch the
                // venv is no longer Creating, so didClose/didChange apply
                // their deferred mutation and then no-op on the forward
                // (venv absent from the pool). A replayed didOpen is the
                // one case that does more than that: `ensure_backend_in_pool`
                // sees the venv absent and starts a fresh creation attempt.
                // That's bounded, not a retry loop — a second failure finds
                // no replay queued for it (only a newly-arrived client
                // message could requeue), and the error notification is
                // already deduplicated — so this just turns "lazy retry on
                // the next unrelated request" into "one immediate retry for
                // documents the client opened during the failed window".
                let mut requeued_notifications = Vec::new();
                for queued in entry.queued {
                    if let Some(id) = &queued.id {
                        self.state.pending_requests.remove(id);
                        let error_response = RpcMessage::error_response(
                            &queued,
                            &format!("lsp-proxy: backend error: {e}"),
                        );
                        client_writer.write_message(&error_response).await?;
                    } else {
                        requeued_notifications.push(queued);
                    }
                }
                self.state.replay_queue.extend(requeued_notifications);
                self.notify_backend_error(&venv_path, &e, client_writer)
                    .await;
            }
        }

        Ok(())
    }

    /// Debounced check for whether a pooled backend's venv identity (derived
    /// from `.venv/pyvenv.cfg` metadata) has changed since it was last
    /// observed. Debounced to once per `venv_check_interval()`; disabled
    /// entirely when the interval is `0`.
    ///
    /// - Token mismatch: the venv was replaced. Evicts the old backend
    ///   (cancelling pending requests, clearing diagnostics, shutting down)
    ///   and starts creating a new one (`Creating`) via the same path
    ///   pool-miss resolution uses. A synchronous failure to even start
    ///   creation is contained as `StaleOutcome::RespawnFailed` — never
    ///   propagated as a fatal error.
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
                // One venv's failed respawn must not take down the whole
                // proxy: the old backend is already evicted, so return the
                // error as an outcome and let the next request retry lazily.
                match self.start_backend_creation(&venv_path, client_writer).await {
                    Ok(()) => Ok(StaleOutcome::Respawned),
                    Err(e) => {
                        tracing::error!(
                            venv = %venv_path.display(),
                            error = ?e,
                            "Respawn after venv replacement failed"
                        );
                        Ok(StaleOutcome::RespawnFailed(e))
                    }
                }
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
