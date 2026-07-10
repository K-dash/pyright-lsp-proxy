use crate::error::BackendError;
use crate::framing::{LspFrameReader, LspFrameWriter};
use crate::message::{RpcId, RpcMessage};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Supported LSP backend types for Python type checking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum BackendKind {
    Pyright,
    Ty,
    Pyrefly,
}

impl BackendKind {
    /// Short name for logging (matches CLI value)
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Pyright => "pyright",
            Self::Ty => "ty",
            Self::Pyrefly => "pyrefly",
        }
    }

    pub fn command(self) -> &'static str {
        match self {
            Self::Pyright => "pyright-langserver",
            Self::Ty => "ty",
            Self::Pyrefly => "pyrefly",
        }
    }

    /// Command name used for `--version` detection.
    /// pyright-langserver does not support `--version`, so we use `pyright` instead.
    pub fn version_command(self) -> &'static str {
        match self {
            Self::Pyright => "pyright",
            Self::Ty => "ty",
            Self::Pyrefly => "pyrefly",
        }
    }

    fn args(self) -> &'static [&'static str] {
        match self {
            Self::Pyright => &["--stdio"],
            Self::Ty => &["server"],
            Self::Pyrefly => &["lsp"],
        }
    }

    /// Apply backend-specific environment variables to the command.
    /// Currently all backends use `VIRTUAL_ENV` + PATH, but this method
    /// provides the extension point for future backend-specific env setup.
    #[allow(clippy::unused_self)] // Intentional extension point; see doc comment above.
    pub fn apply_env(self, cmd: &mut Command, venv: &Path) {
        let venv_str = venv.to_string_lossy();
        cmd.env("VIRTUAL_ENV", venv_str.as_ref());

        let current_path = std::env::var("PATH").unwrap_or_default();
        let new_path = format!("{}/bin:{}", venv_str, current_path);
        cmd.env("PATH", &new_path);
    }
}

impl std::fmt::Display for BackendKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display_name())
    }
}

/// Components returned by `LspBackend::into_split()`
pub struct BackendParts {
    pub reader: LspFrameReader<ChildStdout>,
    pub writer: LspFrameWriter<ChildStdin>,
    pub child: Child,
    pub next_id: u64,
}

pub struct LspBackend {
    child: Child,
    reader: LspFrameReader<ChildStdout>,
    writer: LspFrameWriter<ChildStdin>,
    next_id: u64,
}

impl LspBackend {
    /// Spawn an LSP backend process.
    ///
    /// When `venv_path` is Some, apply backend-specific environment variables.
    pub fn spawn(kind: BackendKind, venv_path: Option<&Path>) -> Result<Self, BackendError> {
        let mut cmd = Command::new(kind.command());
        for arg in kind.args() {
            cmd.arg(arg);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);

        if let Some(venv) = venv_path {
            kind.apply_env(&mut cmd, venv);

            tracing::info!(
                backend = kind.display_name(),
                venv = %venv.display(),
                path_prefix = %format!("{}/bin", venv.display()),
                "Spawning backend with venv"
            );
        } else {
            tracing::warn!(
                backend = kind.display_name(),
                "Spawning backend without venv"
            );
        }

        let mut child = cmd.spawn()?;

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        let reader = LspFrameReader::new(stdout);
        let writer = LspFrameWriter::new(stdin);

        Ok(Self {
            child,
            reader,
            writer,
            next_id: 1,
        })
    }

    /// Send message
    pub async fn send_message(&mut self, message: &RpcMessage) -> Result<(), BackendError> {
        self.writer
            .write_message(message)
            .await
            .map_err(BackendError::Communication)?;
        Ok(())
    }

    /// Receive message
    pub async fn read_message(&mut self) -> Result<RpcMessage, BackendError> {
        self.reader
            .read_message()
            .await
            .map_err(BackendError::Communication)
    }

    /// Split backend into reader, writer, and child process.
    /// Used after initialize handshake to separate reader (for spawned task) from writer.
    pub fn into_split(self) -> BackendParts {
        BackendParts {
            reader: self.reader,
            writer: self.writer,
            child: self.child,
            next_id: self.next_id,
        }
    }
}

/// Fire-and-forget shutdown using only writer + child (reader task is aborted by caller).
/// Spawns a tokio task that:
/// 1. Sends shutdown request → waits 100ms
/// 2. Sends exit notification
/// 3. Waits up to 2s for process exit
/// 4. Kills if still alive
pub fn shutdown_fire_and_forget(
    mut writer: LspFrameWriter<ChildStdin>,
    mut child: Child,
    next_id: u64,
    venv_display: String,
) {
    tokio::spawn(async move {
        tracing::info!(venv = %venv_display, "Starting fire-and-forget shutdown");

        // 1. Send shutdown request
        let shutdown_msg = RpcMessage::request(RpcId::Number(next_id as i64), "shutdown", None);

        if let Err(e) = writer.write_message(&shutdown_msg).await {
            tracing::warn!(venv = %venv_display, error = ?e, "Failed to send shutdown, killing directly");
            let _ = child.kill().await;
            return;
        }

        // 2. Wait briefly for shutdown to be processed
        tokio::time::sleep(Duration::from_millis(100)).await;

        // 3. Send exit notification
        let exit_msg = RpcMessage::notification("exit", None);

        if let Err(e) = writer.write_message(&exit_msg).await {
            tracing::warn!(venv = %venv_display, error = ?e, "Failed to send exit notification");
        }

        // 4. Wait up to 2s for process to exit
        match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
            Ok(Ok(status)) => {
                tracing::info!(venv = %venv_display, status = ?status, "Backend exited gracefully");
            }
            Ok(Err(e)) => {
                tracing::warn!(venv = %venv_display, error = ?e, "Error waiting for backend exit, killing");
                let _ = child.kill().await;
            }
            Err(_) => {
                tracing::warn!(venv = %venv_display, "Backend exit timeout, killing");
                let _ = child.kill().await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_kind_command_and_args() {
        assert_eq!(BackendKind::Pyright.command(), "pyright-langserver");
        assert_eq!(BackendKind::Pyright.args(), &["--stdio"]);
        assert_eq!(BackendKind::Ty.command(), "ty");
        assert_eq!(BackendKind::Ty.args(), &["server"]);
        assert_eq!(BackendKind::Pyrefly.command(), "pyrefly");
        assert_eq!(BackendKind::Pyrefly.args(), &["lsp"]);
    }

    #[test]
    fn backend_kind_display_name() {
        assert_eq!(BackendKind::Pyright.display_name(), "pyright");
        assert_eq!(BackendKind::Ty.display_name(), "ty");
        assert_eq!(BackendKind::Pyrefly.display_name(), "pyrefly");
    }

    #[test]
    fn backend_kind_display_trait() {
        assert_eq!(format!("{}", BackendKind::Pyright), "pyright");
        assert_eq!(format!("{}", BackendKind::Ty), "ty");
        assert_eq!(format!("{}", BackendKind::Pyrefly), "pyrefly");
    }
}
