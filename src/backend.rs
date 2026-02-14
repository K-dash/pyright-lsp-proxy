use crate::error::BackendError;
use crate::framing::{LspFrameReader, LspFrameWriter};
use crate::message::{RpcId, RpcMessage};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Components returned by `PyrightBackend::into_split()`
pub struct BackendParts {
    pub reader: LspFrameReader<ChildStdout>,
    pub writer: LspFrameWriter<ChildStdin>,
    pub child: Child,
    pub next_id: u64,
}

pub struct PyrightBackend {
    child: Child,
    reader: LspFrameReader<ChildStdout>,
    writer: LspFrameWriter<ChildStdin>,
    next_id: u64,
}

impl PyrightBackend {
    /// Spawn pyright-langserver
    ///
    /// When venv_path is Some, set VIRTUAL_ENV and PATH
    pub async fn spawn(venv_path: Option<&Path>) -> Result<Self, BackendError> {
        let mut cmd = Command::new("pyright-langserver");
        cmd.arg("--stdio")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit()) // Inherit stderr to parent (for debugging)
            .kill_on_drop(true);

        // Set environment variables
        if let Some(venv) = venv_path {
            let venv_str = venv.to_string_lossy();

            // Set VIRTUAL_ENV
            cmd.env("VIRTUAL_ENV", venv_str.as_ref());

            // Prepend .venv/bin to PATH
            let current_path = std::env::var("PATH").unwrap_or_default();
            let new_path = format!("{}/bin:{}", venv_str, current_path);
            cmd.env("PATH", &new_path);

            tracing::info!(
                venv = %venv_str,
                path_prefix = %format!("{}/bin", venv_str),
                "Spawning pyright-langserver with venv"
            );
        } else {
            tracing::warn!("Spawning pyright-langserver without venv");
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
            .map_err(|e| BackendError::SpawnFailed(std::io::Error::other(e)))?;
        Ok(())
    }

    /// Receive message
    pub async fn read_message(&mut self) -> Result<RpcMessage, BackendError> {
        self.reader
            .read_message()
            .await
            .map_err(|e| BackendError::SpawnFailed(std::io::Error::other(e)))
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

    /// Gracefully shutdown backend
    ///
    /// 1. Send shutdown request (wait 1-2 seconds)
    /// 2. Send exit notification (wait 1 second)
    /// 3. Wait for process (1 second)
    /// 4. Kill if failed
    #[allow(dead_code)]
    pub async fn shutdown_gracefully(&mut self) -> Result<(), BackendError> {
        let shutdown_id = self.next_id;
        self.next_id += 1;

        tracing::info!(
            shutdown_id = shutdown_id,
            "Sending shutdown request to backend"
        );

        // Send shutdown request
        let shutdown_msg = RpcMessage {
            jsonrpc: "2.0".to_string(),
            id: Some(RpcId::Number(shutdown_id as i64)),
            method: Some("shutdown".to_string()),
            params: None,
            result: None,
            error: None,
        };

        if let Err(e) = self.send_message(&shutdown_msg).await {
            tracing::warn!(error = ?e, "Failed to send shutdown request, will kill directly");
            return self.kill_backend().await;
        }

        // Wait 2 seconds for shutdown response (skip notifications, wait for response)
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                tracing::warn!("Shutdown response timeout");
                break;
            }

            let wait_result = tokio::time::timeout(remaining, self.read_message()).await;

            match wait_result {
                Ok(Ok(msg)) => {
                    // Check if response (has id)
                    if msg.is_response() {
                        // Check if matches shutdown_id
                        if let Some(RpcId::Number(id)) = &msg.id {
                            if *id == shutdown_id as i64 {
                                tracing::info!(response_id = ?msg.id, "Received shutdown response");
                                break;
                            } else {
                                tracing::debug!(response_id = ?msg.id, expected_id = shutdown_id, "Received different response, continuing");
                            }
                        }
                    } else {
                        // Ignore notifications and continue loop
                        tracing::debug!(method = ?msg.method, "Received notification during shutdown, ignoring");
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = ?e, "Error reading shutdown response");
                    break;
                }
                Err(_) => {
                    tracing::warn!("Shutdown response timeout");
                    break;
                }
            }
        }

        // Send exit notification
        let exit_msg = RpcMessage {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: Some("exit".to_string()),
            params: None,
            result: None,
            error: None,
        };

        if let Err(e) = self.send_message(&exit_msg).await {
            tracing::warn!(error = ?e, "Failed to send exit notification");
        }

        tracing::debug!("Sent exit notification, waiting for process to exit");

        // Wait 1 second for process to exit
        let wait_result = tokio::time::timeout(Duration::from_secs(1), self.child.wait()).await;

        match wait_result {
            Ok(Ok(status)) => {
                tracing::info!(status = ?status, "Backend exited gracefully");
                return Ok(());
            }
            Ok(Err(e)) => {
                tracing::warn!(error = ?e, "Error waiting for backend exit");
            }
            Err(_) => {
                tracing::warn!("Backend exit timeout, will kill");
            }
        }

        // Kill if failed
        self.kill_backend().await
    }

    /// Get next ID (for external use, e.g. shutdown messages)
    #[allow(dead_code)]
    pub fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Force kill backend process
    async fn kill_backend(&mut self) -> Result<(), BackendError> {
        tracing::warn!("Killing backend process");

        // Send SIGTERM (use start_kill since kill may not complete async)
        if let Err(e) = self.child.start_kill() {
            tracing::error!(error = ?e, "Failed to kill backend");
            return Err(BackendError::SpawnFailed(std::io::Error::other(
                "Failed to kill backend",
            )));
        }

        // Wait and confirm termination (with timeout)
        let wait_result = tokio::time::timeout(Duration::from_millis(500), self.child.wait()).await;

        match wait_result {
            Ok(Ok(status)) => {
                tracing::info!(status = ?status, "Backend killed successfully");
                Ok(())
            }
            Ok(Err(e)) => {
                tracing::error!(error = ?e, "Error waiting for killed backend");
                Err(BackendError::SpawnFailed(e))
            }
            Err(_) => {
                tracing::error!("Backend kill timeout");
                Err(BackendError::SpawnFailed(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "Backend kill timeout",
                )))
            }
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
        let shutdown_msg = RpcMessage {
            jsonrpc: "2.0".to_string(),
            id: Some(RpcId::Number(next_id as i64)),
            method: Some("shutdown".to_string()),
            params: None,
            result: None,
            error: None,
        };

        if let Err(e) = writer.write_message(&shutdown_msg).await {
            tracing::warn!(venv = %venv_display, error = ?e, "Failed to send shutdown, killing directly");
            let _ = child.kill().await;
            return;
        }

        // 2. Wait briefly for shutdown to be processed
        tokio::time::sleep(Duration::from_millis(100)).await;

        // 3. Send exit notification
        let exit_msg = RpcMessage {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: Some("exit".to_string()),
            params: None,
            result: None,
            error: None,
        };

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
