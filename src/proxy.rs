use crate::backend::PyrightBackend;
use crate::error::ProxyError;
use crate::framing::{LspFrameReader, LspFrameWriter};
use crate::state::ProxyState;
use crate::venv;
use tokio::io::{stdin, stdout};

pub struct LspProxy {
    state: ProxyState,
    debug: bool,
}

impl LspProxy {
    pub fn new(debug: bool) -> Self {
        Self {
            state: ProxyState::new(),
            debug,
        }
    }

    /// メインループ（Phase 3a: fallback env で即座に起動）
    pub async fn run(&mut self) -> Result<(), ProxyError> {
        // stdin/stdout のフレームリーダー/ライター
        let mut client_reader = LspFrameReader::with_debug(stdin(), self.debug);
        let mut client_writer = LspFrameWriter::with_debug(stdout(), self.debug);

        // 起動時 cwd を取得
        let cwd = std::env::current_dir()?;
        tracing::info!(cwd = %cwd.display(), "Starting pyright-lsp-proxy");

        // git toplevel を取得してキャッシュ
        self.state.git_toplevel = venv::get_git_toplevel(&cwd).await?;

        // fallback env を探索
        let fallback_venv = venv::find_fallback_venv(&cwd).await?;

        if let Some(ref venv) = fallback_venv {
            tracing::info!(venv = %venv.display(), "Using fallback .venv");
            self.state.active_venv = Some(venv.clone());
        } else {
            tracing::warn!("No fallback .venv found, starting without venv");
        }

        // backend を起動（fallback env で、なければ venv なし）
        let mut backend = PyrightBackend::spawn(fallback_venv.as_deref(), self.debug).await?;

        let mut didopen_count = 0;

        loop {
            tokio::select! {
                // クライアント（Claude Code）からのメッセージ
                result = client_reader.read_message() => {
                    let msg = result?;
                    let method = msg.method_name();

                    tracing::debug!(
                        method = ?method,
                        is_request = msg.is_request(),
                        is_notification = msg.is_notification(),
                        "Client -> Proxy"
                    );

                    // initialize をキャッシュ（Phase 3b-1: backend 再初期化で流用）
                    if method == Some("initialize") {
                        tracing::info!("Caching initialize message for backend restart");
                        self.state.client_initialize = Some(msg.clone());
                    }

                    // textDocument/didOpen の場合は .venv 探索 & 切替判定
                    if method == Some("textDocument/didOpen") {
                        didopen_count += 1;

                        // Phase 3b-1: 切替が必要なら backend 再起動
                        if let Some(new_backend) = self.handle_did_open(&msg, didopen_count, &mut backend).await? {
                            tracing::info!(session = self.state.backend_session, "Backend switched successfully");
                            backend = new_backend;
                            continue; // didOpen は再起動時に再送済みなのでスキップ
                        }
                    }

                    // backend に転送
                    backend.send_message(&msg).await?;
                }

                // バックエンド（pyright）からのメッセージ
                result = backend.read_message() => {
                    let msg = result?;
                    tracing::debug!(
                        is_response = msg.is_response(),
                        is_notification = msg.is_notification(),
                        "Backend -> Proxy"
                    );

                    // クライアントに転送
                    client_writer.write_message(&msg).await?;
                }
            }
        }
    }

    /// didOpen 処理 & .venv 切替判定（Phase 3b-1）
    ///
    /// 返り値: Some(new_backend) の場合は backend を切替済み、None の場合は切替不要
    async fn handle_did_open(
        &mut self,
        msg: &crate::message::RpcMessage,
        count: usize,
        backend: &mut PyrightBackend,
    ) -> Result<Option<PyrightBackend>, ProxyError> {
        // params から URI と text を抽出
        if let Some(params) = &msg.params {
            if let Some(text_document) = params.get("textDocument") {
                let text = text_document
                    .get("text")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string());

                if let Some(uri_value) = text_document.get("uri") {
                    if let Some(uri_str) = uri_value.as_str() {
                        if let Ok(url) = url::Url::parse(uri_str) {
                            if let Ok(file_path) = url.to_file_path() {
                                tracing::info!(
                                    count = count,
                                    uri = uri_str,
                                    path = %file_path.display(),
                                    has_text = text.is_some(),
                                    text_len = text.as_ref().map(|s| s.len()).unwrap_or(0),
                                    "didOpen received"
                                );

                                // .venv 探索
                                let found_venv = venv::find_venv(
                                    &file_path,
                                    self.state.git_toplevel.as_deref(),
                                )
                                .await?;

                                if let Some(ref venv) = found_venv {
                                    // Phase 3b-1: 切替判定
                                    if self.state.needs_venv_switch(venv) {
                                        tracing::warn!(
                                            current = ?self.state.active_venv.as_ref().map(|p| p.display().to_string()),
                                            found = %venv.display(),
                                            "Venv switch needed, restarting backend"
                                        );

                                        // text が取れなければエラー（MVP では必須）
                                        let text = text.ok_or_else(|| {
                                            ProxyError::InvalidMessage("didOpen missing text field".to_string())
                                        })?;

                                        // last_open を保存
                                        self.state.last_open = Some((url.clone(), text, venv.clone()));

                                        // backend 再起動 & 切替
                                        let new_backend = self.restart_backend_with_venv(backend, venv).await?;

                                        return Ok(Some(new_backend));
                                    } else {
                                        tracing::debug!(
                                            venv = %venv.display(),
                                            "Using same .venv as before"
                                        );
                                    }
                                } else {
                                    tracing::warn!(
                                        path = %file_path.display(),
                                        "No .venv found for this file"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    /// backend を graceful shutdown して新しい .venv で再起動（Phase 3b-1）
    async fn restart_backend_with_venv(
        &mut self,
        backend: &mut PyrightBackend,
        new_venv: &std::path::PathBuf,
    ) -> Result<PyrightBackend, ProxyError> {
        self.state.backend_session += 1;
        let session = self.state.backend_session;

        tracing::info!(
            session = session,
            new_venv = %new_venv.display(),
            "Starting backend restart sequence"
        );

        // 1. 既存 backend を shutdown
        self.state.switching = true;
        if let Err(e) = backend.shutdown_gracefully().await {
            tracing::error!(error = ?e, "Failed to shutdown backend gracefully");
            // エラーでも続行（新 backend 起動を試みる）
        }

        // 2. 新しい backend を起動
        tracing::info!(session = session, venv = %new_venv.display(), "Spawning new backend");
        let mut new_backend = PyrightBackend::spawn(Some(new_venv), self.debug).await?;

        // 3. backend に initialize を送る（プロキシが backend クライアントになる）
        let init_params = self.state.client_initialize.as_ref()
            .and_then(|msg| msg.params.clone())
            .ok_or_else(|| ProxyError::InvalidMessage("No initialize params cached".to_string()))?;

        let init_msg = crate::message::RpcMessage {
            jsonrpc: "2.0".to_string(),
            id: Some(crate::message::RpcId::Number(1)),
            method: Some("initialize".to_string()),
            params: Some(init_params),
            result: None,
            error: None,
        };

        tracing::info!(session = session, "Sending initialize to new backend");
        new_backend.send_message(&init_msg).await?;

        // 4. initialize response を受信（通知はスキップ、id 確認、タイムアウト付き）
        let init_id = 1i64;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(ProxyError::Backend(crate::error::BackendError::SpawnFailed(
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "Initialize response timeout (10s)",
                    )
                )));
            }

            let wait_result = tokio::time::timeout(
                remaining,
                new_backend.read_message()
            ).await;

            match wait_result {
                Ok(Ok(msg)) => {
                    if msg.is_response() {
                        // id が一致するか確認
                        if let Some(crate::message::RpcId::Number(id)) = &msg.id {
                            if *id == init_id {
                                tracing::info!(
                                    session = session,
                                    response_id = ?msg.id,
                                    "Received initialize response from backend"
                                );
                                break;
                            } else {
                                tracing::debug!(
                                    session = session,
                                    response_id = ?msg.id,
                                    expected_id = init_id,
                                    "Received different response, continuing"
                                );
                            }
                        }
                    } else {
                        // 通知は無視してループ継続
                        tracing::debug!(
                            session = session,
                            method = ?msg.method,
                            "Received notification during initialize, ignoring"
                        );
                    }
                }
                Ok(Err(e)) => {
                    return Err(ProxyError::Backend(crate::error::BackendError::SpawnFailed(
                        std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("Error reading initialize response: {}", e),
                        )
                    )));
                }
                Err(_) => {
                    return Err(ProxyError::Backend(crate::error::BackendError::SpawnFailed(
                        std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "Initialize response timeout",
                        )
                    )));
                }
            }
        }

        // 5. initialized notification を送る
        let initialized_msg = crate::message::RpcMessage {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: Some("initialized".to_string()),
            params: Some(serde_json::json!({})),
            result: None,
            error: None,
        };

        tracing::info!(session = session, "Sending initialized to backend");
        new_backend.send_message(&initialized_msg).await?;

        // 6. last_open の didOpen を再送
        if let Some((url, text, _)) = &self.state.last_open {
            let didopen_msg = crate::message::RpcMessage {
                jsonrpc: "2.0".to_string(),
                id: None,
                method: Some("textDocument/didOpen".to_string()),
                params: Some(serde_json::json!({
                    "textDocument": {
                        "uri": url.to_string(),
                        "languageId": "python",
                        "version": 1,
                        "text": text,
                    }
                })),
                result: None,
                error: None,
            };

            tracing::info!(
                session = session,
                uri = %url,
                text_len = text.len(),
                "Resending didOpen to new backend"
            );
            new_backend.send_message(&didopen_msg).await?;
        }

        // 7. 状態更新
        self.state.active_venv = Some(new_venv.clone());
        self.state.switching = false;

        tracing::info!(
            session = session,
            venv = %new_venv.display(),
            "Backend restart completed successfully"
        );

        Ok(new_backend)
    }
}
