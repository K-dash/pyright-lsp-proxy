use crate::backend_pool::BackendMessage;
use crate::error::{BackendError, ProxyError};
use crate::framing::LspFrameWriter;
use crate::message::{RpcId, RpcMessage};
use std::path::PathBuf;

impl super::LspProxy {
    /// Handle a message received from a backend via the mpsc channel.
    ///
    /// This covers the entire `Some(backend_msg) = ...recv()` arm of the
    /// main `tokio::select!` loop.  The caller should `continue` after
    /// this method returns `Ok(())`.
    pub(crate) async fn dispatch_backend_message(
        &mut self,
        backend_msg: BackendMessage,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<(), ProxyError> {
        let BackendMessage {
            venv_path,
            session,
            result,
        } = backend_msg;

        // Current session check: the reader task is already running (and
        // draining) while its venv is still `Creating` — see the #93 fix —
        // so a message can be "current" against either the Ready instance
        // or the in-flight `CreatingEntry`. Checking `backends` alone here
        // silently drops the backend's early responses to restoration
        // (e.g. `publishDiagnostics`), which #93's own acceptance criteria
        // rule out ("no message loss"). `classify_session` is the pure
        // decision extracted for deterministic unit testing (see below) —
        // this is a narrow window to race in an E2E test, so that's where
        // this logic's regression coverage actually lives.
        let ready_session = self.state.pool.get(&venv_path).map(|inst| inst.session);
        let creating_session = self.state.pool.creating_session(&venv_path);
        match classify_session(ready_session, creating_session, session) {
            SessionCurrency::Ready => {
                self.dispatch_ready_backend_message(venv_path, session, result, client_writer)
                    .await
            }
            SessionCurrency::Creating => {
                self.dispatch_creating_backend_message(venv_path, session, result, client_writer)
                    .await
            }
            SessionCurrency::Stale => {
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
                Ok(())
            }
        }
    }

    /// Handle a message from a backend that's still `Creating` (split and
    /// reader-draining, but not yet inserted into `backends`).
    ///
    /// Notifications are the common case here: they're the backend's own
    /// reaction to a restoration `didOpen` (e.g. `publishDiagnostics`), and
    /// forwarding them needs no pending-request/fan-out bookkeeping (that
    /// only applies to requests/responses). A `$/progress` notification's
    /// token is namespaced exactly as it would be for a `Ready` backend
    /// (`progress_token::namespace`), so the client sees consistent tokens
    /// regardless of which side of the Creating/Ready boundary a given
    /// message happened to land on.
    ///
    /// `window/workDoneProgress/create` is the one request kind handled
    /// here (see #104): the backend can start indexing — and emit this
    /// request for its indexing progress — before its own creation task
    /// completes, since the reader task drains from the moment it's split,
    /// well before insertion into `backends`. There is no reachable writer
    /// back to the backend at this point (it's owned by the in-flight
    /// creation task), so the request is still dropped without a response —
    /// but its token is recorded onto `CreatingEntry.indexing_progress_token`
    /// first and carried into the `BackendInstance` on success
    /// (`pool_management::handle_creation_outcome`), so the eventual
    /// progress-based Ready trigger doesn't silently lose track of it. This
    /// is safe by construction even if some backend actually blocks on the
    /// ack before sending `$/progress`: warmup just never sees a
    /// progress-based signal for that session and falls back to the
    /// timeout — degraded, never wrong. pyright 1.1.407 does not wait for
    /// the ack (see #104's capture notes).
    ///
    /// Any other request/response can't legitimately arrive here: no client
    /// request is ever forwarded to a Creating backend (queued instead, in
    /// `CreatingEntry.queued`), and the handshake's own response is consumed
    /// pre-split, inside the creation task, never reaching this channel.
    async fn dispatch_creating_backend_message(
        &mut self,
        venv_path: PathBuf,
        session: u64,
        result: Result<RpcMessage, BackendError>,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<(), ProxyError> {
        match result {
            Ok(mut msg) if msg.is_notification() => {
                if msg.method.as_deref() == Some("$/progress") {
                    if let Some(params) = msg.params.as_mut() {
                        if super::progress_token::namespace(params, session).is_none() {
                            tracing::warn!(
                                venv = %venv_path.display(),
                                session = session,
                                "$/progress notification during Creating has no valid token; forwarding unrewritten"
                            );
                        }
                    }
                }
                client_writer.write_message(&msg).await?;
            }
            Ok(msg)
                if msg.is_request()
                    && msg.method.as_deref() == Some("window/workDoneProgress/create") =>
            {
                if let Some(token) = super::progress_token::extract(msg.params.as_ref()) {
                    if let Some(entry) = self.state.pool.creating_get_mut(&venv_path) {
                        if entry.indexing_progress_token.is_none() {
                            tracing::info!(
                                venv = %venv_path.display(),
                                session = session,
                                token = ?token,
                                "Recording indexing progress token during Creating (request dropped: no reachable backend writer yet)"
                            );
                            entry.indexing_progress_token = Some(token);
                        }
                    }
                } else {
                    tracing::warn!(
                        venv = %venv_path.display(),
                        session = session,
                        "window/workDoneProgress/create during Creating has no valid token"
                    );
                }
            }
            Ok(msg) => {
                tracing::debug!(
                    venv = %venv_path.display(),
                    session = session,
                    is_response = msg.is_response(),
                    is_request = msg.is_request(),
                    "Discarding unexpected request/response from a Creating backend"
                );
            }
            Err(e) => {
                // The backend's reader task died mid-creation. `pool.creating`'s
                // entry is owned by the `creation_rx` completion handler, not
                // this crash-cleanup path (which only operates on `backends`)
                // — the completion handler's own liveness checks catch this:
                // the process itself dying (`try_wait`), the reader task's own
                // `JoinHandle` finishing (`is_finished()`) with the process
                // still alive, or — this arm's own contribution — this exact
                // error, recorded onto `CreatingEntry::reader_error` below so
                // it isn't lost by being consumed (and, before this fix,
                // merely logged) here instead of reaching that check. #106.
                tracing::debug!(
                    venv = %venv_path.display(),
                    session = session,
                    error = ?e,
                    "Backend read error during creation; recording onto the entry for the completion handler"
                );
                if let Some(entry) = self.state.pool.creating_get_mut(&venv_path) {
                    if entry.reader_error.is_none() {
                        entry.reader_error = Some(e);
                    }
                }
            }
        }
        Ok(())
    }

    /// Handle a message from a `Ready` backend (the pre-Creating-state
    /// behavior, unchanged).
    async fn dispatch_ready_backend_message(
        &mut self,
        venv_path: PathBuf,
        session: u64,
        result: Result<RpcMessage, BackendError>,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<(), ProxyError> {
        match result {
            Ok(mut msg) => {
                tracing::debug!(
                    venv = %venv_path.display(),
                    session = session,
                    is_response = msg.is_response(),
                    is_notification = msg.is_notification(),
                    is_request = msg.is_request(),
                    "Backend -> Proxy"
                );

                // Trace-level: log response body for debugging definition/references issues
                if msg.is_response() {
                    tracing::trace!(
                        id = ?msg.id,
                        result = ?msg.result,
                        error = ?msg.error,
                        venv = %venv_path.display(),
                        "Backend response body"
                    );
                }

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
                        self.state
                            .pending_backend_requests
                            .insert(proxy_id.clone(), pending);

                        // Rewrite the ID before forwarding to client
                        let mut forwarded_msg = msg;
                        forwarded_msg.id = Some(proxy_id);

                        // Namespace the token too (see #104): the id above
                        // only dedupes the request/response round-trip
                        // itself, but `params.token` is a separate value the
                        // client keeps around for later `$/progress`
                        // matching and `window/workDoneProgress/cancel` — it
                        // needs the same per-backend namespacing. The first
                        // one observed while the backend is still Warming is
                        // recorded as its indexing progress token, gating
                        // the Ready transition below.
                        if forwarded_msg.method.as_deref() == Some("window/workDoneProgress/create")
                        {
                            if let Some(params) = forwarded_msg.params.as_mut() {
                                match super::progress_token::namespace(params, session) {
                                    Some(original_token) => {
                                        if let Some(inst) = self.state.pool.get_mut(&venv_path) {
                                            if inst.is_warming()
                                                && inst.indexing_progress_token.is_none()
                                            {
                                                tracing::info!(
                                                    venv = %venv_path.display(),
                                                    session = session,
                                                    token = ?original_token,
                                                    "Recording indexing progress token for warmup gating"
                                                );
                                                inst.indexing_progress_token = Some(original_token);
                                            }
                                        }
                                    }
                                    None => {
                                        tracing::warn!(
                                            venv = %venv_path.display(),
                                            session = session,
                                            "window/workDoneProgress/create has no valid token; forwarding unrewritten"
                                        );
                                    }
                                }
                            }
                        }

                        client_writer.write_message(&forwarded_msg).await?;
                    } else {
                        // Request without ID (shouldn't happen per JSON-RPC, but be defensive)
                        client_writer.write_message(&msg).await?;
                    }
                    return Ok(());
                }

                // Handle response: check fan-out first, then pending + stale check
                if msg.is_response() {
                    if let Some(id) = &msg.id {
                        // Fan-out response check: must come before normal pending_requests handling
                        if self.handle_fanout_response(id, &msg, client_writer).await? {
                            return Ok(());
                        }

                        if let Some(pending) = self.state.pending_requests.get(id) {
                            if pending.backend_session != session || pending.venv_path != venv_path
                            {
                                tracing::warn!(
                                    id = ?id,
                                    pending_session = pending.backend_session,
                                    pending_venv = %pending.venv_path.display(),
                                    msg_session = session,
                                    msg_venv = %venv_path.display(),
                                    "Discarding stale response from old backend session"
                                );
                                self.state.pending_requests.remove(id);
                                return Ok(());
                            }
                        } else if is_proxy_assigned_id(id) {
                            // Response for a proxy-assigned ID that is not in pending_requests
                            // and was not consumed by fan-out. This is a stale response from a
                            // cancelled/expired fan-out sub-request — discard it.
                            tracing::debug!(
                                id = ?id,
                                venv = %venv_path.display(),
                                "Discarding stale fan-out sub-request response (already completed/cancelled)"
                            );
                            return Ok(());
                        }
                        self.state.pending_requests.remove(id);
                    }
                }

                // Namespace $/progress tokens (see #104), and gate the
                // warmup Ready transition on an EXACT match against this
                // backend's own recorded indexing token — never on kind
                // alone, which an unrelated progress stream (this backend's
                // or, pre-fix, another backend's colliding one) could
                // trigger early.
                if msg.is_notification() && msg.method.as_deref() == Some("$/progress") {
                    let params = msg.params.as_mut();
                    let original_token =
                        params.and_then(|p| super::progress_token::namespace(p, session));
                    match original_token {
                        Some(original_token) => {
                            if is_progress_end(&msg) {
                                if let Some(inst) = self.state.pool.get_mut(&venv_path) {
                                    if inst.is_warming()
                                        && inst.indexing_progress_token.as_ref()
                                            == Some(&original_token)
                                    {
                                        tracing::info!(
                                            venv = %venv_path.display(),
                                            session = session,
                                            token = ?original_token,
                                            "Backend warmup complete (reason: progress), transitioning to Ready"
                                        );
                                        let queued = inst.mark_ready();
                                        if !queued.is_empty() {
                                            self.drain_warmup_queue(
                                                &venv_path,
                                                session,
                                                queued,
                                                client_writer,
                                            )
                                            .await?;
                                        }
                                    }
                                }
                            }
                        }
                        None => {
                            tracing::warn!(
                                venv = %venv_path.display(),
                                session = session,
                                "$/progress notification has no valid token; forwarding unrewritten, warmup gating skipped"
                            );
                        }
                    }
                }

                // Forward to client
                if msg.is_response() {
                    tracing::trace!(
                        id = ?msg.id,
                        has_result = msg.result.is_some(),
                        has_error = msg.error.is_some(),
                        "Forwarding response to client"
                    );
                }
                client_writer.write_message(&msg).await?;
            }
            Err(e) => {
                tracing::error!(
                    venv = %venv_path.display(),
                    session = session,
                    error = ?e,
                    "Backend read error (crash/EOF)"
                );
                self.handle_backend_crash(&venv_path, session, client_writer)
                    .await?;
            }
        }

        Ok(())
    }
}

/// Check if an RPC ID was assigned by the proxy (negative numbers).
/// Used to detect stale fan-out sub-request responses that should be dropped.
fn is_proxy_assigned_id(id: &RpcId) -> bool {
    matches!(id, RpcId::Number(n) if *n < 0)
}

/// Check if a `$/progress` notification has `params.value.kind == "end"`.
fn is_progress_end(msg: &RpcMessage) -> bool {
    msg.params
        .as_ref()
        .and_then(|p| p.get("value"))
        .and_then(|v| v.get("kind"))
        .and_then(|k| k.as_str())
        == Some("end")
}

/// Where a backend message's session is current, if anywhere.
#[derive(Debug, PartialEq, Eq)]
enum SessionCurrency {
    /// Matches the venv's `Ready` instance.
    Ready,
    /// Matches the venv's in-flight `Creating` entry.
    Creating,
    /// Matches neither — an evicted/crashed/replaced backend's leftover message.
    Stale,
}

/// Pure decision logic for `dispatch_backend_message`'s currency check,
/// extracted so it can be unit tested deterministically. The real bug this
/// guards against (#93 follow-up: messages during the Creating window being
/// misclassified as stale) only reproduces in an E2E test by winning a
/// sub-millisecond race against the creation task's own completion — not a
/// reliable regression signal — so this is where the actual coverage lives.
fn classify_session(
    ready_session: Option<u64>,
    creating_session: Option<u64>,
    msg_session: u64,
) -> SessionCurrency {
    if ready_session == Some(msg_session) {
        SessionCurrency::Ready
    } else if creating_session == Some(msg_session) {
        SessionCurrency::Creating
    } else {
        SessionCurrency::Stale
    }
}

#[cfg(test)]
mod tests {
    use super::{classify_session, SessionCurrency};

    #[test]
    fn matches_ready_session() {
        assert_eq!(classify_session(Some(5), None, 5), SessionCurrency::Ready);
    }

    #[test]
    fn matches_creating_session() {
        // No Ready instance yet (still Creating) — this is exactly the #93
        // follow-up bug: before the fix, this case fell through to Stale.
        assert_eq!(
            classify_session(None, Some(5), 5),
            SessionCurrency::Creating
        );
    }

    #[test]
    fn ready_takes_priority_when_both_present() {
        // Shouldn't normally happen (a venv is Ready XOR Creating, never
        // both), but Ready must win if it ever does.
        assert_eq!(
            classify_session(Some(5), Some(5), 5),
            SessionCurrency::Ready
        );
    }

    #[test]
    fn stale_when_session_matches_neither() {
        assert_eq!(
            classify_session(Some(4), Some(6), 5),
            SessionCurrency::Stale
        );
    }

    #[test]
    fn stale_when_venv_untracked() {
        assert_eq!(classify_session(None, None, 5), SessionCurrency::Stale);
    }
}
