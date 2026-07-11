mod backend_dispatch;
mod client_dispatch;
mod diagnostics;
mod document;
mod fanout;
mod initialization;
mod pool_management;
mod progress_token;

use crate::backend::{BackendKind, LspBackend};
use crate::error::ProxyError;
use crate::framing::{LspFrameReader, LspFrameWriter};
use crate::message::RpcMessage;
use crate::state::ProxyState;
use crate::venv;
use client_dispatch::NotificationDelivery;
use pool_management::StaleOutcome;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{stdin, stdout};
use tokio::time::MissedTickBehavior;

pub struct LspProxy {
    state: ProxyState,
    backend_ttl: Option<Duration>,
}

impl LspProxy {
    pub fn new(
        backend_kind: BackendKind,
        max_backends: usize,
        backend_ttl: Option<Duration>,
    ) -> Self {
        Self {
            state: ProxyState::new(backend_kind, max_backends, backend_ttl),
            backend_ttl,
        }
    }

    /// Dispatch one client-originated message: either read directly from the
    /// client, or replayed from `ProxyState::replay_queue` after arriving
    /// while its venv was `Creating` (see the `creation_rx`/replay select!
    /// arms in `run`). Replayed messages are modeled as ordinary client
    /// messages that arrived early and go through this exact same dispatch —
    /// they can never be `initialize`/`initialized`/`shutdown`/`exit`
    /// (nothing queues those into a Creating entry).
    ///
    /// Returns `Ok(true)` if the proxy should terminate (client sent `exit`).
    async fn dispatch_client_message(
        &mut self,
        msg: RpcMessage,
        pending_initial_backend: &mut Option<(LspBackend, PathBuf)>,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
        didopen_count: &mut usize,
    ) -> Result<bool, ProxyError> {
        let method = msg.method_name();

        tracing::debug!(
            method = ?method,
            is_request = msg.is_request(),
            is_notification = msg.is_notification(),
            "Client -> Proxy"
        );

        // Dispatch based on method, preserving original if-chain order
        match method {
            Some("initialize") => {
                self.dispatch_initialize(&msg, pending_initial_backend, client_writer)
                    .await?;
            }
            Some("initialized") => {
                self.dispatch_initialized().await?;
            }
            Some("shutdown") => {
                self.dispatch_shutdown(&msg, client_writer).await?;
            }
            Some("exit") => {
                tracing::info!("Received exit notification, terminating proxy");
                return Ok(true);
            }
            _ if msg.is_response() && self.dispatch_client_response(&msg).await? => {}
            Some("textDocument/didOpen") => {
                *didopen_count += 1;
                self.handle_did_open(&msg, *didopen_count, client_writer)
                    .await?;
            }
            Some("textDocument/didChange") => {
                let url = Self::extract_text_document_uri(&msg);
                let venv_path = url.as_ref().and_then(|u| self.venv_for_uri(u));

                // Determine BEFORE mutating the cache: if the venv is
                // already Creating, a queued didChange gets replayed later
                // through this exact arm — applying the (possibly
                // range-based, non-idempotent) edit now AND again on replay
                // would double-apply it, corrupting the cached text that
                // future restorations read from. Defer the mutation to
                // whichever pass actually finds the venv no longer Creating
                // (the eventual replay). If the venv is Ready, or about to
                // become Creating as a side effect of THIS message's own
                // staleness check below, the mutation must happen now: a
                // same-tick respawn's restoration snapshot depends on the
                // fresh text already being cached by the time it's taken
                // (see the Respawned arm's comment below).
                let already_creating = venv_path
                    .as_ref()
                    .is_some_and(|v| self.state.pool.creating_contains(v));
                if !already_creating {
                    self.handle_did_change(&msg)?;
                }

                // Forward to appropriate backend, after verifying its venv
                // identity hasn't gone stale.
                if let Some(venv_path) = venv_path {
                    match self
                        .check_pooled_venv_staleness(&venv_path, client_writer)
                        .await?
                    {
                        StaleOutcome::Fresh => {
                            // Ready → forward; Creating → queue (the
                            // silent-drop bug `forward_to_backend` alone
                            // would hit here, since a Creating venv
                            // isn't in `backends` yet); absent → no-op.
                            let delivery = self
                                .forward_or_queue_for_venv(&venv_path, &msg, client_writer)
                                .await?;
                            // The deferred-edit assumption above only holds
                            // for `Queued` — a queue overflow, or (defensively
                            // — structurally unreachable, since nothing
                            // between the `already_creating` check and here
                            // can move the venv out of `creating`) the entry
                            // vanishing outright, both mean nothing will ever
                            // replay this message. Apply the edit now instead:
                            // delivery to the in-flight backend is lost (the
                            // overflow showMessage already told the client
                            // that), but the cache stays coherent for the
                            // next respawn's restoration snapshot. A replayed
                            // didChange re-enters this exact arm, so a queue
                            // that's *still* full on replay takes this same
                            // path automatically.
                            if already_creating {
                                // `Queued` and `Forwarded` are both no-ops
                                // here, but for unrelated reasons (a
                                // guaranteed future replay vs. a structurally
                                // unreachable state), so keep them as
                                // separate documented arms.
                                #[allow(clippy::match_same_arms)]
                                match delivery {
                                    NotificationDelivery::Queued => {}
                                    NotificationDelivery::Dropped
                                    | NotificationDelivery::NoBackend => {
                                        self.handle_did_change(&msg)?;
                                    }
                                    // Structurally unreachable alongside
                                    // `already_creating`: `Forwarded` requires
                                    // `pool.contains`, which was false at the
                                    // check above and can't change mid-arm.
                                    NotificationDelivery::Forwarded => {}
                                }
                            }
                        }
                        // Respawned: creation just started for this venv
                        // (it was Ready a moment ago, hence `already_creating`
                        // was false above), so handle_did_change already
                        // updated the cache above, and the fresh text is in
                        // the restoration snapshot the new creation task
                        // took. Removed: revival-preparation mode, nothing
                        // to forward to.
                        StaleOutcome::Respawned | StaleOutcome::Removed => {}
                        // Respawn failure is contained: notify the
                        // client; the next request retries lazily.
                        StaleOutcome::RespawnFailed(e) => {
                            self.notify_backend_error(&venv_path, &e, client_writer)
                                .await;
                        }
                    }
                }
            }
            Some("textDocument/didClose") => {
                // Get venv before removing from cache
                let venv_for_close =
                    Self::extract_text_document_uri(&msg).and_then(|url| self.venv_for_uri(&url));

                // If the venv is already Creating, this didClose gets
                // queued and replayed later through this exact arm. Do NOT
                // remove the doc from the cache yet in that case: this same
                // `venv_for_uri` lookup above is what the replay pass needs
                // to determine where to route the didClose, and removing
                // the entry now would make it unresolvable on replay,
                // silently losing the didClose a second time (the bug this
                // whole arm exists to fix). Defer the cache removal to
                // whichever pass actually delivers it — immediately here if
                // Ready, or on replay once the venv is no longer Creating.
                let is_creating = venv_for_close
                    .as_ref()
                    .is_some_and(|v| self.state.pool.creating_contains(v));
                if !is_creating {
                    self.handle_did_close(&msg);
                }

                // Forward to appropriate backend: Ready -> immediate
                // forward, as before. Creating -> queue behind the
                // restoration snapshot replay. The snapshot was taken
                // before this didClose could have arrived, so the creation
                // task still replays the (now logically closed) document's
                // didOpen — without queueing this didClose too, the backend
                // would permanently hold a document the proxy considers
                // closed (stale diagnostics; a double-didOpen if it's
                // reopened later). `forward_to_backend` alone (writes only
                // to a Ready instance) would silently drop this case.
                if let Some(venv_path) = venv_for_close {
                    let delivery = self
                        .forward_or_queue_for_venv(&venv_path, &msg, client_writer)
                        .await?;
                    // Same reasoning as the didChange arm above: `Queued` is
                    // the only outcome guaranteed a future replay. A queue
                    // overflow (`Dropped`), or defensively `NoBackend`, means
                    // nothing will ever redeliver this didClose, so the
                    // deferred cache removal must happen now instead — the
                    // ghost document would otherwise poison every future
                    // restoration snapshot for this venv. A replayed
                    // didClose re-enters this exact arm, so a queue that's
                    // still full on replay takes this same path
                    // automatically.
                    if is_creating {
                        // `Queued` and `Forwarded` are both no-ops here, but
                        // for unrelated reasons — see the didChange arm's
                        // identical comment above.
                        #[allow(clippy::match_same_arms)]
                        match delivery {
                            NotificationDelivery::Queued => {}
                            NotificationDelivery::Dropped | NotificationDelivery::NoBackend => {
                                self.handle_did_close(&msg);
                            }
                            // Structurally unreachable alongside
                            // `is_creating`: see the didChange arm's
                            // identical comment above.
                            NotificationDelivery::Forwarded => {}
                        }
                    }
                }
            }
            Some("$/cancelRequest") => {
                self.dispatch_cancel_request(&msg, client_writer).await?;
            }
            Some("window/workDoneProgress/cancel") => {
                self.dispatch_workdone_progress_cancel(&msg).await?;
            }
            _ if msg.is_request() => {
                self.dispatch_client_request(&msg, client_writer).await?;
            }
            _ if msg.is_notification() => {
                self.dispatch_client_notification(&msg, client_writer)
                    .await?;
            }
            _ => {}
        }

        Ok(false)
    }

    pub async fn run(&mut self) -> Result<(), ProxyError> {
        let mut client_writer = LspFrameWriter::new(stdout());

        // Client reads happen on a dedicated task, not inline in the
        // select! below. `LspFrameReader::read_message()` spans multiple
        // `.await` points internally (a `read_line` loop for headers, then
        // `read_exact` for the body), and `read_line` is documented by
        // tokio as NOT cancellation-safe: if some other select! branch
        // (backend traffic, a timer, replay) resolves first while a client
        // read is mid-frame, the read future gets dropped, discarding
        // whatever header/body bytes it had already pulled out of the
        // stream and permanently desyncing framing for every subsequent
        // read. Under sustained client traffic racing backend responses
        // this reproduces as spurious "Missing Content-Length header"
        // errors that crash the whole proxy — trivial to hit once the
        // select! is no longer `biased;` client-first (see above), since
        // the client read future no longer gets first refusal every poll
        // cycle. Backend reads already use this exact
        // dedicated-task-plus-channel shape for the identical reason (see
        // `backend_pool::spawn_reader_task`): `mpsc::Receiver::recv()` IS
        // cancel-safe, so consuming from a channel inside select! is safe
        // even though the read that fills it isn't.
        // Bounded generously (not just the `1` cancel-safety alone would
        // need): a tight `1` slot forces the reader task to fully
        // read-parse-send each message before starting the next, which
        // throttles how often `client_msg_rx` is actually ready and, as a
        // side effect, masks a reintroduced `biased;` client-first
        // regression (client can't monopolize a channel it can barely keep
        // non-empty). 256 lets the reader task race ahead and keep the
        // channel topped up under sustained input, roughly matching how
        // many small frames the OS pipe + `BufReader` could already hold
        // ready for the old inline read to consume in one shot — so a
        // `biased;` regression is exactly as starvation-prone here as it
        // was before this task existed.
        let (client_msg_tx, mut client_msg_rx) =
            tokio::sync::mpsc::channel::<Result<RpcMessage, crate::error::FramingError>>(256);
        tokio::spawn(async move {
            let mut client_reader = LspFrameReader::new(stdin());
            loop {
                let result = client_reader.read_message().await;
                let is_err = result.is_err();
                if client_msg_tx.send(result).await.is_err() {
                    // Main loop gone (proxy shutting down).
                    break;
                }
                if is_err {
                    // Read error (EOF, malformed frame) already sent; stop.
                    break;
                }
            }
        });

        let cwd = std::env::current_dir()?;
        tracing::info!(
            cwd = %cwd.display(),
            backend = self.state.backend_kind.display_name(),
            max_backends = self.state.pool.max_backends(),
            backend_ttl = ?self.backend_ttl.map(|d| format!("{}s", d.as_secs())),
            "Starting LSP proxy"
        );

        // Get and cache git toplevel
        self.state.git_toplevel = venv::get_git_toplevel(&cwd).await?;

        // Search for fallback venv
        let fallback_venv = venv::find_fallback_venv(&cwd).await?;

        // Pre-spawn backend if fallback venv found (but don't insert into pool yet —
        // wait for client's `initialize` to complete the handshake first)
        let mut pending_initial_backend: Option<(LspBackend, PathBuf)> = if let Some(venv) =
            fallback_venv
        {
            tracing::info!(venv = %venv.display(), "Using fallback .venv, pre-spawning backend");
            let backend = LspBackend::spawn(self.state.backend_kind, Some(&venv))?;
            Some((backend, venv))
        } else {
            tracing::warn!("No fallback .venv found, starting with empty pool");
            None
        };

        let mut didopen_count: usize = 0;

        // TTL sweep timer: checks every 60 seconds for expired backends
        let mut ttl_interval = tokio::time::interval(std::time::Duration::from_secs(60));
        ttl_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // Consume the first immediate tick so the first real tick fires after 60s
        ttl_interval.tick().await;

        loop {
            // Compute deadlines before entering select! to avoid borrow conflicts
            let warmup_deadline = self.state.pool.nearest_warmup_deadline();
            let fanout_deadline = self.state.nearest_fanout_deadline();

            tokio::select! {

                // Unbiased (default randomized-order polling): client,
                // backend, creation-completion, timers, and replay all get
                // a fair shot each iteration. A `biased;` client-first order
                // was tried and reverted — under sustained client input it
                // starves everything else (backend responses, creation
                // outcomes, every timer, replay), and can reintroduce the
                // #93 deadlock: client flood + a diagnostics burst fills
                // backend_msg_rx, parking the reader task; the wedged
                // backend stops reading its own stdin; a client-arm forward
                // (e.g. didChange) then blocks writing to that backend's
                // stdin, and the main loop is stuck with no other arm ever
                // getting polled to drain the channel and unwedge it.
                //
                // The backend-before-replay ordering (still required — see
                // ARCHITECTURE.md's Creating-state section) is enforced
                // locally inside the replay arm below instead, via a
                // non-blocking `try_recv` on backend_msg_rx.

                // Messages from client (see the dedicated reader task set
                // up above `run`'s select! loop for why this isn't read
                // inline here).
                Some(result) = client_msg_rx.recv() => {
                    let msg = result?;
                    if self.dispatch_client_message(msg, &mut pending_initial_backend, &mut client_writer, &mut didopen_count).await? {
                        return Ok(());
                    }
                }

                // Messages from all backends via mpsc channel
                Some(backend_msg) = self.state.pool.backend_msg_rx.recv() => {
                    self.dispatch_backend_message(backend_msg, &mut client_writer).await?;
                }

                // Backend-creation task completion (success or failure).
                // Fast bookkeeping only: insert into the pool / move queued
                // messages into `replay_queue` on success, or send bounded
                // error responses + a dedup'd notification on failure. Never
                // does backend I/O for the newly-created instance itself.
                Some(outcome) = self.state.pool.creation_rx.recv() => {
                    self.handle_creation_outcome(outcome, &mut client_writer).await?;
                }

                // TTL-based auto-eviction and venv-identity staleness sweep.
                // Always armed (not gated on backend_ttl) so the staleness
                // sweep runs even when TTL eviction is disabled.
                _ = ttl_interval.tick() => {
                    if self.backend_ttl.is_some() {
                        self.evict_expired_backends(&mut client_writer).await?;
                    }
                    if !crate::backend_pool::venv_check_interval().is_zero() {
                        self.sweep_stale_venvs(&mut client_writer).await?;
                    }
                }

                // Warmup timeout: fail-open transition for warming backends
                () = async {
                    match warmup_deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    self.expire_warmup_backends(&mut client_writer).await?;
                }

                // Fan-out timeout: return partial results for timed-out fan-out requests
                () = async {
                    match fanout_deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    self.expire_fanout_requests(&mut client_writer).await?;
                }

                // Replay queue: re-dispatch exactly ONE message queued while
                // its venv was Creating, per loop iteration — never a batch
                // loop (a burst of replayed didOpens could otherwise exceed
                // backend_msg_rx's capacity and reintroduce the #93
                // deadlock).
                //
                // Backend traffic still gets priority over replay, but that
                // ordering can no longer come from the outer select!'s arm
                // order (no `biased;` — see above), so it's enforced locally
                // here with a non-blocking check before replaying: if
                // backend_msg_rx already has a message ready, dispatch THAT
                // instead and defer the replay to a later iteration (push it
                // back to the front of the queue — it was already popped by
                // this arm's guard).
                //
                // This can leak at most one replay ahead of a backend
                // message that arrives in the gap between the try_recv and
                // this arm actually running (the message wasn't there yet
                // when checked, but is by the time replay dispatch starts) —
                // that's fine: a replayed didOpen's diagnostics burst racing
                // against a single already-queued backend message can't
                // recreate batch saturation, only a genuine batch loop can.
                Some(replay_msg) = async { self.state.replay_queue.pop_front() }, if !self.state.replay_queue.is_empty() => {
                    match self.state.pool.backend_msg_rx.try_recv() {
                        Ok(backend_msg) => {
                            self.state.replay_queue.push_front(replay_msg);
                            self.dispatch_backend_message(backend_msg, &mut client_writer).await?;
                        }
                        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                            if self.dispatch_client_message(replay_msg, &mut pending_initial_backend, &mut client_writer, &mut didopen_count).await? {
                                return Ok(());
                            }
                        }
                        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                            // Unreachable in practice: `BackendPool` holds
                            // its own `backend_msg_tx` clone for the whole
                            // proxy lifetime (see `BackendPool::new`), so
                            // the channel can never actually disconnect
                            // while the pool exists. Fail open rather than
                            // silently dropping the replay if this ever
                            // does trigger.
                            tracing::debug!("backend_msg_rx disconnected while checking before replay (unexpected)");
                            if self.dispatch_client_message(replay_msg, &mut pending_initial_backend, &mut client_writer, &mut didopen_count).await? {
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }
    }
}
