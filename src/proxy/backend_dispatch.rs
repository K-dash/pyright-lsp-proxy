use crate::backend_pool::BackendMessage;
use crate::error::ProxyError;
use crate::framing::LspFrameWriter;
use crate::message::RpcMessage;

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

        // Stale session check: discard messages from backends no longer in the pool
        // or whose session has changed (evicted and re-created)
        let is_current = self
            .state
            .pool
            .get(&venv_path)
            .is_some_and(|inst| inst.session == session);

        if !is_current {
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
            return Ok(());
        }

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

                // Handle response: check pending + stale check
                if msg.is_response() {
                    if let Some(id) = &msg.id {
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

/// Check if a `$/progress` notification has `params.value.kind == "end"`.
fn is_progress_end(msg: &RpcMessage) -> bool {
    msg.params
        .as_ref()
        .and_then(|p| p.get("value"))
        .and_then(|v| v.get("kind"))
        .and_then(|k| k.as_str())
        == Some("end")
}
