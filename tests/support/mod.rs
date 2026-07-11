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
    writer: LspFrameWriter<tokio::process::ChildStdin>,
    #[allow(dead_code)]
    temp_dir: TempDir,
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
        let mut child = Command::new(proxy_bin)
            .current_dir(cwd)
            // Clear git env vars so the proxy's `git rev-parse` uses the test
            // workspace's .git, not the outer repo's (important when running
            // inside pre-commit hooks that set GIT_DIR/GIT_WORK_TREE).
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
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
            writer: LspFrameWriter::new(stdin),
            temp_dir,
            root,
            next_id: 1,
        }
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
        self.writer.write_message(msg).await.unwrap_or_else(|e| {
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
