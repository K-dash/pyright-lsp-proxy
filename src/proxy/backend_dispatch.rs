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
    /// Notifications are the only message kind that can legitimately arrive
    /// here: they're the backend's own reaction to a restoration `didOpen`
    /// (e.g. `publishDiagnostics`), and forwarding them needs no pending-
    /// request/fan-out bookkeeping (that only applies to requests/responses)
    /// and no warmup instance (there isn't one yet — a `$/progress end`
    /// landing here is a no-op per the accepted v1 gap; fail-open still
    /// converges once the backend is `Ready`). So "the normal notification
    /// path" for this window is simply: forward it.
    async fn dispatch_creating_backend_message(
        &self,
        venv_path: PathBuf,
        session: u64,
        result: Result<RpcMessage, BackendError>,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<(), ProxyError> {
        match result {
            Ok(msg) if msg.is_notification() => {
                client_writer.write_message(&msg).await?;
            }
            Ok(msg) => {
                // Requests/responses can't legitimately arrive here: no
                // client request is ever forwarded to a Creating backend
                // (queued instead, in `CreatingEntry.queued`), and the
                // handshake's own response is consumed pre-split, inside
                // the creation task, never reaching this channel.
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
                // — the completion handler's own liveness check catches this
                // and reports it as a contained creation failure, whether the
                // process itself died (`try_wait`) or only the reader task did
                // while the process stayed alive (`reader_task.is_finished()`,
                // #106 — a framing error, or any other read failure, leaves a
                // live process with nothing left to drain its stdout).
                tracing::debug!(
                    venv = %venv_path.display(),
                    session = session,
                    error = ?e,
                    "Backend read error during creation; completion handler will report the outcome"
                );
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
            Ok(msg) => {
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

                // Detect $/progress end → transition warming backend to ready
                if msg.is_notification() {
                    if let Some(method) = msg.method_name() {
                        if method == "$/progress" && is_progress_end(&msg) {
                            if let Some(inst) = self.state.pool.get_mut(&venv_path) {
                                if inst.is_warming() {
                                    tracing::info!(
                                        venv = %venv_path.display(),
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
