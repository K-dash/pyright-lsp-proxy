//! Test harness for E2E tests.
//!
//! Provides `ProxyUnderTest`, a subprocess wrapper that speaks LSP framing
//! to the proxy binary, plus workspace setup helpers.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tempfile::TempDir;
use tokio::process::{Child, Command};
use typemux_cc::framing::{LspFrameReader, LspFrameWriter};
use typemux_cc::message::{RpcId, RpcMessage};

const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

// ── Hermeticity defaults ────────────────────────────────────────────
//
// Every spawned proxy is isolated from the developer's real environment
// (see #120): `HOME` is redirected to a per-spawn temp dir so
// `~/.config/typemux-cc/config` (src/config.rs:21-23) can't leak developer
// settings into a test run, `PATH` is sanitized so a missing mock shim in
// `.venv/bin` fails loudly instead of silently falling through to a real
// `pyright-langserver`/`ty`/`pyrefly` install, and `RUST_LOG` gets a quiet
// default instead of the crate's own `typemux_cc=debug` fallback. Applied
// before `envs`, so an explicit entry in a test's `spawn_with_env` call
// always overrides the default for that key.

/// Minimal `PATH` for spawned proxies: enough to resolve `git` (venv
/// boundary detection) and to exec the mock shim's `#!/bin/sh` shebang
/// (resolved directly by the kernel, not via `PATH`). Excludes any
/// developer-installed backend so a missing shim in `.venv/bin` is a loud,
/// deterministic failure rather than a silent fallback to a real install.
const HERMETIC_PATH: &str = "/usr/bin:/bin";

/// Default `RUST_LOG` for spawned proxies. Quiets the crate's own
/// `typemux_cc=debug` fallback (src/main.rs) and a developer's
/// `~/.config/typemux-cc/config` possibly setting `RUST_LOG=trace`, either
/// of which can push timing-sensitive E2E tests toward their read timeout.
const HERMETIC_RUST_LOG: &str = "typemux_cc=warn";

// ── Workspace configuration ────────────────────────────────────────

/// Describes one package (sub-directory) inside the test workspace.
pub struct PackageConfig {
    pub name: String,
    /// The scenario JSON the mock backend will play back.
    pub scenario: Value,
    /// Whether this package has a `.venv`.
    pub has_venv: bool,
}

/// Describes the entire test workspace layout.
pub struct WorkspaceConfig {
    pub packages: Vec<PackageConfig>,
}

// ── Setup helpers ───────────────────────────────────────────────────

/// Create a temp workspace directory with git init, venvs, scenario files,
/// and fake `pyright-langserver` scripts.  Returns the `TempDir` handle
/// (keeps it alive) and the path to use as cwd for the proxy.
pub fn setup_test_workspace(config: &WorkspaceConfig) -> (TempDir, PathBuf) {
    let temp = TempDir::new().expect("failed to create temp dir");
    // Canonicalize to resolve symlinks (e.g., /var → /private/var on macOS).
    // Without this, git toplevel (/private/var/...) won't match file paths
    // (/var/...) and venv search breaks at the boundary check.
    let root = temp
        .path()
        .canonicalize()
        .expect("failed to canonicalize temp dir");

    // Minimal git repo so git rev-parse works.
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
    std::fs::create_dir_all(root.join(".git/objects")).unwrap();

    for pkg in &config.packages {
        let pkg_dir = root.join(&pkg.name);
        std::fs::create_dir_all(&pkg_dir).unwrap();

        if pkg.has_venv {
            write_venv_fixture(&pkg_dir, &pkg.scenario);
        }
    }

    (temp, root)
}

/// Create (or recreate) `<pkg_dir>/.venv` with a `pyvenv.cfg`, a
/// `scenario.json` for the mock backend, and a fake `pyright-langserver`
/// shim that bridges to `mock-lsp-backend`.
///
/// Used both by `setup_test_workspace` (initial fixture) and by tests that
/// simulate `uv sync` recreating `.venv` mid-run (venv identity tracking).
pub fn write_venv_fixture(pkg_dir: &Path, scenario: &Value) {
    let mock_backend_bin = env!("CARGO_BIN_EXE_mock-lsp-backend");

    let venv_dir = pkg_dir.join(".venv");
    std::fs::create_dir_all(venv_dir.join("bin")).unwrap();
    std::fs::write(venv_dir.join("pyvenv.cfg"), "home = /usr/bin\n").unwrap();

    // Write scenario file into the venv.
    let scenario_json = serde_json::to_string_pretty(scenario).unwrap();
    std::fs::write(venv_dir.join("scenario.json"), &scenario_json).unwrap();

    // Fake pyright-langserver that bridges to mock-lsp-backend.
    let script = format!(
        "#!/bin/sh\nexport MOCK_LSP_SCENARIO_FILE=\"$VIRTUAL_ENV/scenario.json\"\nexec \"{}\" \"$@\"\n",
        mock_backend_bin
    );
    let script_path = venv_dir.join("bin/pyright-langserver");
    std::fs::write(&script_path, &script).unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// Create (or recreate) `<pkg_dir>/.venv` whose fake `pyright-langserver`
/// exits immediately, so backend creation fails deterministically at the
/// initialize handshake. Used to exercise respawn-failure containment.
#[allow(dead_code)] // Used by some but not all integration test binaries.
pub fn write_broken_venv_fixture(pkg_dir: &Path) {
    let venv_dir = pkg_dir.join(".venv");
    std::fs::create_dir_all(venv_dir.join("bin")).unwrap();
    // Different content from `write_venv_fixture`'s pyvenv.cfg so the
    // identity token differs even under an inode/mtime collision.
    std::fs::write(
        venv_dir.join("pyvenv.cfg"),
        "home = /usr/bin\nbroken = marker\n",
    )
    .unwrap();

    let script_path = venv_dir.join("bin/pyright-langserver");
    std::fs::write(&script_path, "#!/bin/sh\nexit 1\n").unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

// ── ProxyUnderTest ──────────────────────────────────────────────────

/// A running proxy process with LSP framing readers/writers attached.
pub struct ProxyUnderTest {
    child: Child,
    reader: LspFrameReader<tokio::process::ChildStdout>,
    // `Option` so the writer half can be taken and handed to a separately
    // spawned task (see `spawn_notification_flood`) while `self` keeps the
    // reader half for observing responses. `None` after that happens; any
    // subsequent call routing through `write()` panics.
    writer: Option<LspFrameWriter<tokio::process::ChildStdin>>,
    #[allow(dead_code)]
    temp_dir: TempDir,
    // Kept alive for the proxy's lifetime: its path backs the spawned
    // proxy's `HOME`, which `src/config.rs` reads at startup.
    #[allow(dead_code)]
    harness_home: TempDir,
    #[allow(dead_code)] // Read via `root()`, used by some but not all integration test binaries.
    root: PathBuf,
    next_id: i64,
}

impl ProxyUnderTest {
    /// Spawn the proxy binary with the given workspace as cwd.
    #[allow(dead_code)] // Used by some but not all integration test binaries.
    pub fn spawn(temp_dir: TempDir, root: PathBuf, cwd: &Path) -> Self {
        Self::spawn_with_env(temp_dir, root, cwd, &[])
    }

    /// Spawn the proxy binary with the given workspace as cwd and additional
    /// environment variables (e.g. `TYPEMUX_CC_VENV_CHECK_INTERVAL`).
    pub fn spawn_with_env(
        temp_dir: TempDir,
        root: PathBuf,
        cwd: &Path,
        envs: &[(&str, &str)],
    ) -> Self {
        let proxy_bin = env!("CARGO_BIN_EXE_typemux-cc");
        let harness_home = TempDir::new().expect("failed to create harness HOME dir");

        let mut child = Command::new(proxy_bin)
            .current_dir(cwd)
            // Clear git env vars so the proxy's `git rev-parse` uses the test
            // workspace's .git, not the outer repo's (important when running
            // inside pre-commit hooks that set GIT_DIR/GIT_WORK_TREE).
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            // Hermeticity defaults (see the module-level comment above);
            // `envs` is applied last so an explicit entry always wins.
            .env("HOME", harness_home.path())
            .env("PATH", HERMETIC_PATH)
            .env("RUST_LOG", HERMETIC_RUST_LOG)
            .envs(envs.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("failed to spawn proxy");

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        Self {
            child,
            reader: LspFrameReader::new(stdout),
            writer: Some(LspFrameWriter::new(stdin)),
            temp_dir,
            harness_home,
            root,
            next_id: 1,
        }
    }

    /// Take the writer half and hand it to a spawned task that floods the
    /// proxy with fire-and-forget `method`/`params` notifications, as fast
    /// as it can, until the returned handle is aborted.
    ///
    /// Used to keep client input continuously pending so a finite
    /// pre-written batch (which an unfair, client-first select! could still
    /// fully drain before any starvation became observable, since stdin
    /// eventually empties) can't mask a fairness regression.
    ///
    /// After this call, `self` can no longer send — anything routing
    /// through `write()` (`request`/`notify`/`send_request`/etc.) panics.
    /// Only reading (`read_next`/`wait_for_response`/...) remains valid.
    /// Intended for tests that only need to observe responses during the
    /// flood and don't need a graceful `shutdown`/`exit` afterward — the
    /// child process is cleaned up by `Drop` regardless.
    #[allow(dead_code)] // Used by some but not all integration test binaries.
    pub fn spawn_notification_flood(
        &mut self,
        method: &'static str,
        params: Value,
    ) -> tokio::task::JoinHandle<()> {
        use tokio::io::AsyncWriteExt;

        // Pre-frame ONE message's bytes once, then repeat them into a
        // single large buffer written via one `write_all` call per batch,
        // instead of looping `write_message` (header/body/flush — three
        // separate `.await` points per message). A per-message loop yields
        // to the scheduler often enough that the proxy's reader can drain
        // the pipe faster than the flood refills it, leaving windows where
        // client_reader.read_message() isn't immediately ready — exactly
        // the gap a starvation regression would otherwise hide in. A large
        // batch keeps the pipe genuinely saturated.
        const BATCH_SIZE: usize = 256;

        let mut writer = self
            .writer
            .take()
            .expect("writer already taken by an earlier flood");

        let msg = RpcMessage::notification(method, Some(params));
        let content = serde_json::to_vec(&msg).expect("flood message must serialize");
        let header = format!("Content-Length: {}\r\n\r\n", content.len());
        let mut one_frame = Vec::with_capacity(header.len() + content.len());
        one_frame.extend_from_slice(header.as_bytes());
        one_frame.extend_from_slice(&content);

        let mut batch = Vec::with_capacity(one_frame.len() * BATCH_SIZE);
        for _ in 0..BATCH_SIZE {
            batch.extend_from_slice(&one_frame);
        }

        tokio::spawn(async move {
            loop {
                if writer.get_mut().write_all(&batch).await.is_err() {
                    return;
                }
            }
        })
    }

    /// Return the canonical workspace root path.
    #[allow(dead_code)] // Used by some but not all integration test binaries.
    pub fn root(&self) -> &Path {
        &self.root
    }

    // ── LSP helpers ─────────────────────────────────────────────────

    /// Send an initialize request and return the response.
    pub async fn initialize(&mut self, root_uri: &str) -> RpcMessage {
        let params = serde_json::json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {},
            "workspaceFolders": [
                { "uri": root_uri, "name": "test" }
            ]
        });
        self.request("initialize", params).await
    }

    /// Send `initialized` notification.
    pub async fn send_initialized(&mut self) {
        let msg = RpcMessage::notification("initialized", Some(serde_json::json!({})));
        self.write(&msg).await;
    }

    /// Send `textDocument/didOpen` notification.
    #[allow(dead_code)] // Used by some but not all integration test binaries.
    pub async fn did_open(&mut self, uri: &str, text: &str) {
        let msg = RpcMessage::notification(
            "textDocument/didOpen",
            Some(serde_json::json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "python",
                    "version": 1,
                    "text": text
                }
            })),
        );
        self.write(&msg).await;
    }

    /// Send `textDocument/didChange` notification (full-sync single change).
    #[allow(dead_code)] // Used by some but not all integration test binaries.
    pub async fn did_change(&mut self, uri: &str, version: i64, text: &str) {
        let msg = RpcMessage::notification(
            "textDocument/didChange",
            Some(serde_json::json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [ { "text": text } ]
            })),
        );
        self.write(&msg).await;
    }

    /// Send an arbitrary fire-and-forget notification.
    #[allow(dead_code)] // Used by some but not all integration test binaries.
    pub async fn notify(&mut self, method: &str, params: Value) {
        let msg = RpcMessage::notification(method, Some(params));
        self.write(&msg).await;
    }

    /// Send a request without waiting for its response. Returns the assigned
    /// id, for matching against a later `read_next`.
    #[allow(dead_code)] // Used by some but not all integration test binaries.
    pub async fn send_request(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        let msg = RpcMessage::request(RpcId::Number(id), method, Some(params));
        self.write(&msg).await;
        id
    }

    /// Send a request and wait for the response (with timeout).
    pub async fn request(&mut self, method: &str, params: Value) -> RpcMessage {
        let id = self.next_id;
        self.next_id += 1;
        let msg = RpcMessage::request(RpcId::Number(id), method, Some(params));
        self.write(&msg).await;
        // Read responses, skipping notifications, until we get one with our id.
        loop {
            let resp = self.read_next().await;
            if resp.is_response() {
                if let Some(RpcId::Number(resp_id)) = &resp.id {
                    if *resp_id == id {
                        return resp;
                    }
                }
            }
            // Otherwise keep reading (notifications, etc.)
        }
    }

    /// Send a request and wait for the response, collecting any notifications
    /// observed in the meantime (with timeout).
    #[allow(dead_code)] // Used by some but not all integration test binaries.
    pub async fn request_collecting(
        &mut self,
        method: &str,
        params: Value,
    ) -> (RpcMessage, Vec<RpcMessage>) {
        let id = self.next_id;
        self.next_id += 1;
        let msg = RpcMessage::request(RpcId::Number(id), method, Some(params));
        self.write(&msg).await;
        let mut notifications = Vec::new();
        loop {
            let resp = self.read_next().await;
            if resp.is_response() {
                if let Some(RpcId::Number(resp_id)) = &resp.id {
                    if *resp_id == id {
                        return (resp, notifications);
                    }
                }
            } else {
                notifications.push(resp);
            }
        }
    }

    /// Perform shutdown + exit sequence. Returns the shutdown response.
    #[allow(dead_code)] // Used by some but not all integration test binaries.
    pub async fn shutdown_and_exit(&mut self) -> RpcMessage {
        let resp = self.request("shutdown", Value::Null).await;
        let exit_msg = RpcMessage::notification("exit", None);
        self.write(&exit_msg).await;
        resp
    }

    /// Wait for crash cleanup to complete by observing `publishDiagnostics` notifications.
    ///
    /// Reads messages until `expected_diag_count` publishDiagnostics notifications
    /// with empty diagnostics arrays are received, or the absolute deadline expires.
    /// Panics if a non-notification message (response) is received unexpectedly.
    #[allow(dead_code)] // Used by some but not all integration test binaries.
    pub async fn wait_for_crash_cleanup(
        &mut self,
        expected_diag_count: usize,
        timeout_ms: u64,
    ) -> Vec<RpcMessage> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        let mut collected = Vec::new();
        let mut diag_count = 0;

        while diag_count < expected_diag_count {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "wait_for_crash_cleanup: timed out after {}ms, got {}/{} diagnostics",
                timeout_ms,
                diag_count,
                expected_diag_count
            );
            // The outer `Err` case is `tokio::time::error::Elapsed`, which carries no
            // information beyond "timed out" — already covered by the panic message below.
            #[allow(clippy::match_wild_err_arm)]
            match tokio::time::timeout(remaining, self.reader.read_message()).await {
                Ok(Ok(msg)) => {
                    assert!(
                        msg.is_notification(),
                        "wait_for_crash_cleanup: unexpected non-notification: {:?}",
                        msg
                    );
                    if msg.method.as_deref() == Some("textDocument/publishDiagnostics") {
                        if let Some(params) = &msg.params {
                            if let Some(diags) = params.get("diagnostics") {
                                if diags.as_array().is_some_and(std::vec::Vec::is_empty) {
                                    diag_count += 1;
                                }
                            }
                        }
                    }
                    collected.push(msg);
                }
                Ok(Err(e)) => {
                    let stderr = self.dump_stderr().await;
                    panic!("wait_for_crash_cleanup: read error: {e}\n--- stderr ---\n{stderr}");
                }
                Err(_) => {
                    panic!(
                        "wait_for_crash_cleanup: timed out after {}ms, got {}/{} diagnostics",
                        timeout_ms, diag_count, expected_diag_count
                    );
                }
            }
        }
        collected
    }

    /// Wait for a notification with the given method, discarding any other
    /// notifications observed in the meantime. Panics on timeout.
    #[allow(dead_code)] // Used by some but not all integration test binaries.
    pub async fn wait_for_notification(&mut self, method: &str, timeout_ms: u64) -> RpcMessage {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "wait_for_notification: timed out after {timeout_ms}ms waiting for {method:?}"
            );
            match tokio::time::timeout(remaining, self.reader.read_message()).await {
                Ok(Ok(msg)) if msg.method.as_deref() == Some(method) => return msg,
                Ok(Ok(_)) => {} // Not the notification we're waiting for.
                Ok(Err(e)) => {
                    let stderr = self.dump_stderr().await;
                    panic!("wait_for_notification: read error: {e}\n--- stderr ---\n{stderr}");
                }
                Err(_) => {
                    let stderr = self.dump_stderr().await;
                    panic!(
                        "wait_for_notification: timed out after {timeout_ms}ms waiting for {method:?}\n--- stderr ---\n{stderr}"
                    );
                }
            }
        }
    }

    /// Wait for a response matching `id`, discarding any other messages
    /// (notifications, other responses) observed in the meantime. For
    /// requests sent via `send_request` whose response may not be the very
    /// next message (e.g. it's raced against another venv's unrelated
    /// traffic). Panics on timeout.
    #[allow(dead_code)] // Used by some but not all integration test binaries.
    pub async fn wait_for_response(&mut self, id: i64, timeout_ms: u64) -> RpcMessage {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "wait_for_response: timed out after {timeout_ms}ms waiting for id {id}"
            );
            match tokio::time::timeout(remaining, self.reader.read_message()).await {
                Ok(Ok(msg)) if msg.is_response() && msg.id == Some(RpcId::Number(id)) => {
                    return msg;
                }
                Ok(Ok(_)) => {} // Not the response we're waiting for.
                Ok(Err(e)) => {
                    let stderr = self.dump_stderr().await;
                    panic!("wait_for_response: read error: {e}\n--- stderr ---\n{stderr}");
                }
                Err(_) => {
                    let stderr = self.dump_stderr().await;
                    panic!(
                        "wait_for_response: timed out after {timeout_ms}ms waiting for id {id}\n--- stderr ---\n{stderr}"
                    );
                }
            }
        }
    }

    /// Collect any messages that arrive within `wait_ms`, without panicking
    /// if none arrive before the deadline. Used to give an async side effect
    /// (e.g. backend-crash cleanup notifications) a bounded window to
    /// surface, so a test can assert on its absence.
    #[allow(dead_code)] // Used by some but not all integration test binaries.
    pub async fn drain_notifications(&mut self, wait_ms: u64) -> Vec<RpcMessage> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(wait_ms);
        let mut collected = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return collected;
            }
            match tokio::time::timeout(remaining, self.reader.read_message()).await {
                Ok(Ok(msg)) => collected.push(msg),
                Ok(Err(e)) => panic!("drain_notifications: framing error: {e}"),
                Err(_) => return collected,
            }
        }
    }

    /// Read the next LSP message, with no built-in timeout — the caller
    /// wraps this in its own `tokio::time::timeout` (e.g. against an
    /// overall deadline shared across many reads, rather than `read_next`'s
    /// fixed per-call timeout).
    #[allow(dead_code)] // Used by some but not all integration test binaries.
    pub async fn read_message_raw(&mut self) -> RpcMessage {
        match self.reader.read_message().await {
            Ok(msg) => msg,
            Err(e) => {
                let stderr = self.dump_stderr().await;
                panic!("read_message_raw: framing error: {e}\n--- proxy stderr ---\n{stderr}");
            }
        }
    }

    /// Read the next LSP message (with timeout).
    pub async fn read_next(&mut self) -> RpcMessage {
        match tokio::time::timeout(READ_TIMEOUT, self.reader.read_message()).await {
            Ok(Ok(msg)) => msg,
            Ok(Err(e)) => {
                let stderr = self.dump_stderr().await;
                panic!("read_next: framing error: {e}\n--- proxy stderr ---\n{stderr}");
            }
            Err(_) => {
                let stderr = self.dump_stderr().await;
                panic!(
                    "read_next: timed out after {}s waiting for message\n--- proxy stderr ---\n{stderr}",
                    READ_TIMEOUT.as_secs()
                );
            }
        }
    }

    /// Write an LSP message to the proxy's stdin.
    async fn write(&mut self, msg: &RpcMessage) {
        self.writer
            .as_mut()
            .expect(
                "writer taken by spawn_notification_flood — this ProxyUnderTest can no longer send",
            )
            .write_message(msg)
            .await
            .unwrap_or_else(|e| {
                panic!("write: failed to write message: {e}");
            });
    }

    /// Dump whatever is currently available on the proxy's stderr.
    async fn dump_stderr(&mut self) -> String {
        use tokio::io::AsyncReadExt;
        if let Some(stderr) = self.child.stderr.as_mut() {
            let mut buf = vec![0u8; 16384];
            match tokio::time::timeout(std::time::Duration::from_millis(100), stderr.read(&mut buf))
                .await
            {
                Ok(Ok(n)) => String::from_utf8_lossy(&buf[..n]).to_string(),
                _ => "(could not read stderr)".to_string(),
            }
        } else {
            "(no stderr handle)".to_string()
        }
    }

    /// Continuously drain and discard the proxy's stderr in the background.
    ///
    /// `child.stderr` is a `Stdio::piped()` handle with a small kernel
    /// buffer (~64KB); nothing else reads it during a test unless a test
    /// calls `dump_stderr` on a failure path. A test that generates high
    /// log volume while the child runs (e.g. one `WARN` per dropped
    /// message under a continuous flood) can fill that buffer within
    /// seconds — `tracing`'s stderr writer is synchronous, so once the
    /// pipe is full the proxy blocks on its own logging and every task on
    /// its runtime stalls with it, indistinguishable from a genuine
    /// starvation/deadlock bug from the outside. Tests that generate
    /// sustained traffic should call this right after spawning. Takes
    /// `child.stderr`, so `dump_stderr` becomes a no-op for the rest of
    /// this instance's life — draining and inspecting are mutually
    /// exclusive.
    #[allow(dead_code)] // Used by tests with sustained/high-volume traffic.
    pub fn spawn_stderr_drain(&mut self) -> tokio::task::JoinHandle<()> {
        use tokio::io::AsyncReadExt;
        let mut stderr = self.child.stderr.take().expect("stderr already taken");
        tokio::spawn(async move {
            let mut buf = [0u8; 8192];
            loop {
                match stderr.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        })
    }
}

impl Drop for ProxyUnderTest {
    fn drop(&mut self) {
        // Best-effort kill to avoid orphaned processes on panic.
        let _ = self.child.start_kill();
    }
}

// ── Utility ─────────────────────────────────────────────────────────

/// Convert a filesystem path to a `file://` URI.
pub fn path_to_uri(path: &Path) -> String {
    url::Url::from_file_path(path)
        .expect("path_to_uri: invalid path")
        .to_string()
}
