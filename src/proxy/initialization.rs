use crate::backend::{BackendKind, LspBackend};
use crate::backend_pool::{spawn_reader_task, BackendInstance, BackendMessage, CreationOutcome};
use crate::error::ProxyError;
use crate::framing::LspFrameWriter;
use crate::message::{RpcId, RpcMessage};
use crate::state::OpenDocument;
use serde_json::Value;
use std::path::{Path, PathBuf};
use tokio::process::ChildStdin;
use tokio::sync::mpsc;
use url::Url;

/// Rewrite rootUri, rootPath, and workspaceFolders in initialize params
/// to point to the venv's parent directory (the project root).
///
/// This ensures each backend indexes only the project that owns the venv,
/// which is critical for worktree paths (dot-prefixed directories like
/// `.worktree/` are excluded from indexing when rootUri points to the
/// main repo root).
fn rewrite_root_uri(init_params: &mut Value, venv: &Path) {
    let project_root = match venv.parent() {
        Some(p) => p,
        None => return,
    };

    let root_uri = match Url::from_file_path(project_root) {
        Ok(u) => u.to_string(),
        Err(()) => return,
    };

    let dir_name = project_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace");

    let root_path = project_root.to_string_lossy().to_string();

    tracing::info!(
        root_uri = %root_uri,
        root_path = %root_path,
        "Rewriting initialize params rootUri to venv project root"
    );

    if let Some(obj) = init_params.as_object_mut() {
        obj.insert("rootUri".to_string(), Value::String(root_uri.clone()));
        obj.insert("rootPath".to_string(), Value::String(root_path));
        obj.insert(
            "workspaceFolders".to_string(),
            serde_json::json!([{"uri": root_uri, "name": dir_name}]),
        );
    }
}

/// Whether an open document should be restored (re-sent as `didOpen`) to a
/// freshly (re)spawned backend for `venv`.
///
/// A document restores here if it already routes to this venv, or if its
/// venv was never resolved (`doc_venv` is `None`) and the file falls under
/// this venv's project root. A document already owned by a *different* venv
/// is never restored here, even if its path is nested under this venv's
/// project root (e.g. a child project's own `.venv`).
///
/// `pub(crate)`: also called from `pool_management::start_backend_creation`
/// to build the restoration snapshot on the main-loop side, before the
/// creation task is spawned.
pub(crate) fn should_restore_document(
    doc_venv: Option<&Path>,
    venv: &Path,
    file_path: Option<&Path>,
    venv_parent: Option<&Path>,
) -> bool {
    doc_venv == Some(venv)
        || (doc_venv.is_none()
            && match (file_path, venv_parent) {
                (Some(fp), Some(vp)) => fp.starts_with(vp),
                _ => false,
            })
}

/// Perform the LSP initialize handshake with a backend:
/// 1. Send `initialize` request with the given params
/// 2. Wait for the initialize response (`init_handshake_timeout()`, skip notifications)
/// 3. Send `initialized` notification
///
/// Returns the initialize response from the backend.
async fn perform_initialize_handshake(
    backend: &mut LspBackend,
    mut init_params: Value,
    venv: &Path,
) -> Result<RpcMessage, ProxyError> {
    rewrite_root_uri(&mut init_params, venv);
    tracing::trace!(
        venv = %venv.display(),
        init_params = %init_params,
        "Full initialize params sent to backend"
    );
    let init_msg = RpcMessage::request(RpcId::Number(1), "initialize", Some(init_params));

    tracing::info!(venv = %venv.display(), "Sending initialize to backend");
    backend.send_message(&init_msg).await?;

    // Receive initialize response
    let init_id = 1i64;
    let timeout_duration = crate::backend_pool::init_handshake_timeout();
    let timeout_secs = timeout_duration.as_secs();
    let deadline = tokio::time::Instant::now() + timeout_duration;
    let init_response = loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(ProxyError::Backend(
                crate::error::BackendError::InitializeTimeout(timeout_secs),
            ));
        }

        let wait_result = tokio::time::timeout(remaining, backend.read_message()).await;

        match wait_result {
            Ok(Ok(msg)) => {
                if msg.is_response() {
                    if let Some(RpcId::Number(id)) = &msg.id {
                        if *id == init_id {
                            if let Some(error) = &msg.error {
                                return Err(ProxyError::Backend(
                                    crate::error::BackendError::InitializeResponseError(format!(
                                        "code={}, message={}",
                                        error.code, error.message
                                    )),
                                ));
                            }
                            tracing::info!(
                                venv = %venv.display(),
                                "Received initialize response from backend"
                            );
                            break msg;
                        }
                    }
                } else {
                    tracing::debug!(
                        method = ?msg.method,
                        "Received notification during initialize, ignoring"
                    );
                }
            }
            Ok(Err(e)) => {
                return Err(ProxyError::Backend(
                    crate::error::BackendError::InitializeFailed(format!(
                        "Error reading initialize response: {}",
                        e
                    )),
                ));
            }
            Err(_) => {
                return Err(ProxyError::Backend(
                    crate::error::BackendError::InitializeTimeout(timeout_secs),
                ));
            }
        }
    };

    // Send initialized notification
    let initialized_msg = RpcMessage::notification("initialized", Some(serde_json::json!({})));

    tracing::info!(venv = %venv.display(), "Sending initialized to backend");
    backend.send_message(&initialized_msg).await?;

    Ok(init_response)
}

impl super::LspProxy {
    /// Extract cached initialize params, returning an error if not available.
    ///
    /// `pub(crate)`: also called from `pool_management::start_backend_creation`
    /// to snapshot the params before spawning the creation task.
    pub(crate) fn cached_init_params(&self) -> Result<Value, ProxyError> {
        self.state
            .client_initialize
            .as_ref()
            .and_then(|msg| msg.params.clone())
            .ok_or_else(|| ProxyError::InvalidMessage("No initialize params cached".to_string()))
    }

    /// Complete backend initialization: forward initialize, receive response, send initialized.
    /// Returns the initialize response to forward to the client.
    ///
    /// Used only by the very first (pre-spawned fallback) backend at startup,
    /// before the event loop is even running — no other venv can be waiting
    /// on the loop yet, so this stays inline rather than moving into a
    /// spawned task like `start_backend_creation`.
    pub(crate) async fn complete_backend_initialization(
        &self,
        backend: &mut LspBackend,
        venv: &Path,
        _client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<RpcMessage, ProxyError> {
        let init_params = self.cached_init_params()?;
        perform_initialize_handshake(backend, init_params, venv).await
    }
}

/// Write restored documents (already filtered to this venv by the caller) to
/// a split backend's writer.
///
/// Called only from `run_backend_creation`, AFTER the reader task has
/// started draining the backend's stdout (see `BackendInstance::from_parts`'s
/// doc comment) — the #93 fix: a restoration-triggered diagnostics burst is
/// drained concurrently instead of filling the pipe and deadlocking this
/// write loop.
///
/// Fails fast on the first write error instead of logging and continuing: a
/// write failure here means the pipe is broken (the backend died), so later
/// writes in the loop would fail identically, and returning `Err` fails the
/// whole creation through `CreationOutcome::Err`'s existing containment
/// (queued requests get a JSON-RPC error, one dedup'd notify, lazy retry) —
/// instead of silently pooling an instance with some documents missing.
async fn write_restored_documents(
    writer: &mut LspFrameWriter<ChildStdin>,
    docs: &[(Url, OpenDocument)],
    session: u64,
) -> Result<(), ProxyError> {
    tracing::info!(
        session = session,
        total_docs = docs.len(),
        "Starting document restoration"
    );

    for (url, doc) in docs {
        let uri_str = url.to_string();
        let text_len = doc.text.len();

        let didopen_msg = RpcMessage::notification(
            "textDocument/didOpen",
            Some(serde_json::json!({
                "textDocument": {
                    "uri": uri_str,
                    "languageId": doc.language_id,
                    "version": doc.version,
                    "text": doc.text,
                }
            })),
        );

        writer.write_message(&didopen_msg).await.map_err(|e| {
            tracing::error!(
                session = session,
                uri = %uri_str,
                error = ?e,
                "Failed to restore document"
            );
            e
        })?;

        tracing::info!(
            session = session,
            uri = %uri_str,
            text_len = text_len,
            "Restored document"
        );
    }

    tracing::info!(
        session = session,
        restored = docs.len(),
        "Document restoration completed"
    );
    Ok(())
}

/// Spawn a backend, complete the handshake, split it, start its reader task,
/// replay restored documents, and capture its venv identity token.
///
/// Runs entirely off the event loop (see `spawn_backend_creation_task`), so
/// a slow handshake or a restoration diagnostics burst never blocks the main
/// loop or other venvs' traffic (#93, #96).
pub(crate) async fn run_backend_creation(
    backend_kind: BackendKind,
    venv: &Path,
    session: u64,
    init_params: Value,
    restore_snapshot: Vec<(Url, OpenDocument)>,
    msg_sender: mpsc::Sender<BackendMessage>,
) -> Result<BackendInstance, ProxyError> {
    tracing::info!(session = session, venv = %venv.display(), "Creating new backend instance");

    let mut backend = LspBackend::spawn(backend_kind, Some(venv))?;

    perform_initialize_handshake(&mut backend, init_params, venv).await?;
    tracing::info!(session = session, venv = %venv.display(), "Backend initialized");

    // Split and start the reader task BEFORE restoring: the backend's
    // stdout is drained from this point on, so restoration writes below
    // can't deadlock even under a large diagnostics burst (#93).
    let parts = backend.into_split();
    let reader_task = spawn_reader_task(parts.reader, msg_sender, venv.to_path_buf(), session);

    let mut writer = parts.writer;
    if let Err(e) = write_restored_documents(&mut writer, &restore_snapshot, session).await {
        // The child is dropped (and killed via `kill_on_drop`) when `parts`
        // goes out of scope on this early return; the reader task is
        // aborted explicitly rather than left to notice EOF on its own.
        reader_task.abort();
        return Err(e);
    }

    // Capture venv identity token (None if capture raced a rebuild)
    let venv_token = crate::venv::venv_token(venv).await;

    Ok(BackendInstance::from_parts(
        writer,
        parts.child,
        parts.next_id,
        reader_task,
        venv.to_path_buf(),
        session,
        venv_token,
    ))
}

/// Spawn the backend-creation task and report its outcome back to the main
/// loop via `creation_tx`. Fire-and-forget from the caller's perspective
/// (`pool_management::start_backend_creation` does not await this) — the
/// main loop picks up the result later via the `creation_rx` select! arm.
pub(crate) fn spawn_backend_creation_task(
    backend_kind: BackendKind,
    venv: PathBuf,
    session: u64,
    init_params: Value,
    restore_snapshot: Vec<(Url, OpenDocument)>,
    msg_sender: mpsc::Sender<BackendMessage>,
    creation_tx: mpsc::Sender<CreationOutcome>,
) {
    tokio::spawn(async move {
        let result = run_backend_creation(
            backend_kind,
            &venv,
            session,
            init_params,
            restore_snapshot,
            msg_sender,
        )
        .await;

        let outcome = CreationOutcome {
            venv_path: venv,
            result,
        };
        // Receiver only drops on proxy shutdown; a send failure there is
        // moot (nothing left to report the outcome to).
        let _ = creation_tx.send(outcome).await;
    });
}

#[cfg(test)]
mod tests {
    use super::should_restore_document;
    use std::path::Path;

    const VENV: &str = "/repo/.venv";
    const CHILD_VENV: &str = "/repo/sub/.venv";
    const VENV_PARENT: &str = "/repo";

    #[test]
    fn restores_document_owned_by_this_venv() {
        assert!(should_restore_document(
            Some(Path::new(VENV)),
            Path::new(VENV),
            Some(Path::new("/repo/a.py")),
            Some(Path::new(VENV_PARENT)),
        ));
    }

    #[test]
    fn does_not_restore_document_owned_by_a_different_venv_even_if_nested_under_this_root() {
        // Regression test for #94: a document under `repo/sub/app.py` owned
        // by `repo/sub/.venv` must not be captured by `repo/.venv`'s restore,
        // even though its path starts with `repo`.
        assert!(!should_restore_document(
            Some(Path::new(CHILD_VENV)),
            Path::new(VENV),
            Some(Path::new("/repo/sub/app.py")),
            Some(Path::new(VENV_PARENT)),
        ));
    }

    #[test]
    fn restores_unresolved_document_under_this_venvs_project_root() {
        assert!(should_restore_document(
            None,
            Path::new(VENV),
            Some(Path::new("/repo/a.py")),
            Some(Path::new(VENV_PARENT)),
        ));
    }

    #[test]
    fn does_not_restore_unresolved_document_outside_this_venvs_project_root() {
        assert!(!should_restore_document(
            None,
            Path::new(VENV),
            Some(Path::new("/elsewhere/a.py")),
            Some(Path::new(VENV_PARENT)),
        ));
    }

    #[test]
    fn does_not_restore_unresolved_document_when_venv_has_no_parent() {
        assert!(!should_restore_document(
            None,
            Path::new(VENV),
            Some(Path::new("/repo/a.py")),
            None,
        ));
    }

    /// Deterministic coverage for the write-failure layer of the zombie-
    /// backend fix (a restoration write failure must fail creation instead
    /// of being logged and swallowed — see `run_backend_creation`'s doc
    /// comment). Spawns a trivial process and waits for it to exit before
    /// writing, so the pipe is genuinely, unambiguously closed — unlike the
    /// E2E `dead_backend_during_creation_does_not_pool_as_zombie_e2e`, which
    /// races a mock's own crash timing and can't reliably isolate this path
    /// from the pre-existing Ready-backend crash-cleanup path that also
    /// catches most timings of that race.
    #[tokio::test]
    async fn write_restored_documents_fails_on_broken_pipe() {
        use super::write_restored_documents;
        use crate::framing::LspFrameWriter;
        use crate::state::OpenDocument;
        use url::Url;

        let mut child = tokio::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("exit 0")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn helper process");
        let stdin = child.stdin.take().expect("child stdin should be piped");
        // Wait for exit before writing: guarantees the pipe's read end is
        // closed, so the write below fails deterministically (EPIPE) rather
        // than racing the child's own exit.
        child
            .wait()
            .await
            .expect("helper process should exit cleanly");

        let mut writer = LspFrameWriter::new(stdin);
        let docs = vec![(
            Url::parse("file:///a.py").unwrap(),
            OpenDocument {
                language_id: "python".to_string(),
                version: 1,
                text: "a = 1\n".to_string(),
                venv: None,
            },
        )];

        let result = write_restored_documents(&mut writer, &docs, 1).await;
        assert!(
            result.is_err(),
            "expected a restoration write to an already-closed pipe to fail, not be silently swallowed"
        );
    }
}
