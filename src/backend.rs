use crate::error::BackendError;
use crate::framing::{LspFrameReader, LspFrameWriter};
use crate::message::{RpcId, RpcMessage};
use std::path::Path;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use std::process::Stdio;
use std::time::Duration;

pub struct PyrightBackend {
    child: Child,
    reader: LspFrameReader<ChildStdout>,
    writer: LspFrameWriter<ChildStdin>,
    next_id: u64,
}

impl PyrightBackend {
    /// pyright-langserver を起動
    ///
    /// venv_path が Some の場合、VIRTUAL_ENV と PATH を設定
    pub async fn spawn(venv_path: Option<&Path>, debug: bool) -> Result<Self, BackendError> {
        let mut cmd = Command::new("pyright-langserver");
        cmd.arg("--stdio")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit()) // stderr は親に継承（デバッグ用）
            .kill_on_drop(true);

        // 環境変数設定
        if let Some(venv) = venv_path {
            let venv_str = venv.to_string_lossy();

            // VIRTUAL_ENV を設定
            cmd.env("VIRTUAL_ENV", venv_str.as_ref());

            // PATH の先頭に .venv/bin を追加
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

        let reader = LspFrameReader::with_debug(stdout, debug);
        let writer = LspFrameWriter::with_debug(stdin, debug);

        Ok(Self {
            child,
            reader,
            writer,
            next_id: 1,
        })
    }

    /// メッセージを送信
    pub async fn send_message(&mut self, message: &RpcMessage) -> Result<(), BackendError> {
        self.writer
            .write_message(message)
            .await
            .map_err(|e| BackendError::SpawnFailed(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        Ok(())
    }

    /// メッセージを受信
    pub async fn read_message(&mut self) -> Result<RpcMessage, BackendError> {
        self.reader
            .read_message()
            .await
            .map_err(|e| BackendError::SpawnFailed(std::io::Error::new(std::io::ErrorKind::Other, e)))
    }

    /// backend を graceful shutdown する（Phase 3b-1）
    ///
    /// 1. shutdown request 送信（1〜2秒待つ）
    /// 2. exit notification 送信（1秒待つ）
    /// 3. プロセス wait（1秒待つ）
    /// 4. ダメなら kill
    pub async fn shutdown_gracefully(&mut self) -> Result<(), BackendError> {
        let shutdown_id = self.next_id;
        self.next_id += 1;

        tracing::info!(shutdown_id = shutdown_id, "Sending shutdown request to backend");

        // shutdown request 送信
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

        // shutdown response を 2秒待つ（通知はスキップしてレスポンスを待つ）
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
                    // レスポンス（id あり）か確認
                    if msg.is_response() {
                        // shutdown_id と一致するか確認
                        if let Some(RpcId::Number(id)) = &msg.id {
                            if *id == shutdown_id as i64 {
                                tracing::info!(response_id = ?msg.id, "Received shutdown response");
                                break;
                            } else {
                                tracing::debug!(response_id = ?msg.id, expected_id = shutdown_id, "Received different response, continuing");
                            }
                        }
                    } else {
                        // 通知は無視してループ継続
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

        // exit notification 送信
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

        // プロセス wait を 1秒待つ
        let wait_result = tokio::time::timeout(
            Duration::from_secs(1),
            self.child.wait()
        ).await;

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

        // ダメなら kill
        self.kill_backend().await
    }

    /// backend プロセスを強制終了
    async fn kill_backend(&mut self) -> Result<(), BackendError> {
        tracing::warn!("Killing backend process");

        // SIGTERM を送る（kill が非同期で完了しない可能性があるので start_kill）
        if let Err(e) = self.child.start_kill() {
            tracing::error!(error = ?e, "Failed to kill backend");
            return Err(BackendError::SpawnFailed(
                std::io::Error::new(std::io::ErrorKind::Other, "Failed to kill backend")
            ));
        }

        // wait して終了を確認（タイムアウト付き）
        let wait_result = tokio::time::timeout(
            Duration::from_millis(500),
            self.child.wait()
        ).await;

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
                Err(BackendError::SpawnFailed(
                    std::io::Error::new(std::io::ErrorKind::TimedOut, "Backend kill timeout")
                ))
            }
        }
    }
}
