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

                        // Phase 3b-2: 切替が必要なら backend 再起動
                        if let Some(new_backend) = self.handle_did_open(&msg, didopen_count, &mut backend).await? {
                            tracing::info!(session = self.state.backend_session, "Backend switched successfully");
                            backend = new_backend;
                            continue; // didOpen は再起動時に再送済みなのでスキップ
                        }
                    }

                    // textDocument/didChange の場合は text を更新（Phase 3b-2）
                    if method == Some("textDocument/didChange") {
                        self.handle_did_change(&msg).await?;
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
                                // languageId と version を取得
                                let language_id = text_document
                                    .get("languageId")
                                    .and_then(|l| l.as_str())
                                    .unwrap_or("unknown")
                                    .to_string();

                                let version = text_document
                                    .get("version")
                                    .and_then(|v| v.as_i64())
                                    .unwrap_or(0) as i32;

                                tracing::info!(
                                    count = count,
                                    uri = uri_str,
                                    path = %file_path.display(),
                                    has_text = text.is_some(),
                                    text_len = text.as_ref().map(|s| s.len()).unwrap_or(0),
                                    language_id = %language_id,
                                    version = version,
                                    "didOpen received"
                                );

                                // Phase 3b-2: didOpen をキャッシュ
                                if let Some(text_content) = &text {
                                    let doc = crate::state::OpenDocument {
                                        uri: url.clone(),
                                        language_id: language_id.clone(),
                                        version,
                                        text: text_content.clone(),
                                    };
                                    self.state.open_documents.insert(url.clone(), doc);
                                    tracing::debug!(
                                        uri = %url,
                                        doc_count = self.state.open_documents.len(),
                                        "Document cached"
                                    );
                                }

                                // .venv 探索
                                let found_venv = venv::find_venv(
                                    &file_path,
                                    self.state.git_toplevel.as_deref(),
                                )
                                .await?;

                                if let Some(ref venv) = found_venv {
                                    // Phase 3b-2: 切替判定
                                    if self.state.needs_venv_switch(venv) {
                                        tracing::warn!(
                                            current = ?self.state.active_venv.as_ref().map(|p| p.display().to_string()),
                                            found = %venv.display(),
                                            "Venv switch needed, restarting backend"
                                        );

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
                return Err(ProxyError::Backend(
                    crate::error::BackendError::InitializeTimeout(10)
                ));
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
                                // error レスポンスか確認
                                if let Some(error) = &msg.error {
                                    return Err(ProxyError::Backend(
                                        crate::error::BackendError::InitializeResponseError(
                                            format!("code={}, message={}", error.code, error.message)
                                        )
                                    ));
                                }

                                tracing::info!(
                                    session = session,
                                    response_id = ?msg.id,
                                    "Received initialize response from backend"
                                );

                                // textDocumentSync capability をログ出力（Phase 3b-2）
                                if let Some(result) = &msg.result {
                                    if let Some(capabilities) = result.get("capabilities") {
                                        if let Some(sync) = capabilities.get("textDocumentSync") {
                                            tracing::info!(
                                                session = session,
                                                text_document_sync = ?sync,
                                                "Backend textDocumentSync capability"
                                            );
                                        }
                                    }
                                }

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
                    return Err(ProxyError::Backend(
                        crate::error::BackendError::InitializeFailed(
                            format!("Error reading initialize response: {}", e)
                        )
                    ));
                }
                Err(_) => {
                    return Err(ProxyError::Backend(
                        crate::error::BackendError::InitializeTimeout(10)
                    ));
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

        // 6. 全ドキュメント復元（Phase 3b-2）
        let total_docs = self.state.open_documents.len();
        let mut restored = 0;
        let mut failed = 0;

        tracing::info!(
            session = session,
            total_docs = total_docs,
            "Starting document restoration"
        );

        for (url, doc) in &self.state.open_documents {
            // 先に必要な値をコピー（await 前に借用終了させる）
            let uri_str = url.to_string();
            let language_id = doc.language_id.clone();
            let version = doc.version;
            let text = doc.text.clone();
            let text_len = text.len();

            let didopen_msg = crate::message::RpcMessage {
                jsonrpc: "2.0".to_string(),
                id: None,
                method: Some("textDocument/didOpen".to_string()),
                params: Some(serde_json::json!({
                    "textDocument": {
                        "uri": uri_str,
                        "languageId": language_id,
                        "version": version,
                        "text": text,
                    }
                })),
                result: None,
                error: None,
            };

            match new_backend.send_message(&didopen_msg).await {
                Ok(_) => {
                    restored += 1;
                    tracing::info!(
                        session = session,
                        uri = %uri_str,
                        version = version,
                        text_len = text_len,
                        "Successfully restored document"
                    );
                }
                Err(e) => {
                    failed += 1;
                    tracing::error!(
                        session = session,
                        uri = %uri_str,
                        error = ?e,
                        "Failed to restore document, skipping"
                    );
                    // Continue with next document (partial restoration strategy)
                }
            }
        }

        tracing::info!(
            session = session,
            restored = restored,
            failed = failed,
            total = total_docs,
            "Document restoration completed"
        );

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

    /// didChange 処理（Phase 3b-2）
    async fn handle_did_change(
        &mut self,
        msg: &crate::message::RpcMessage,
    ) -> Result<(), ProxyError> {
        if let Some(params) = &msg.params {
            if let Some(text_document) = params.get("textDocument") {
                if let Some(uri_str) = text_document.get("uri").and_then(|u| u.as_str()) {
                    if let Ok(url) = url::Url::parse(uri_str) {
                        // textDocument から version を取得（LSP の version を信頼）
                        let version = text_document
                            .get("version")
                            .and_then(|v| v.as_i64())
                            .map(|v| v as i32);

                        // contentChanges から text を取得（full sync 前提）
                        if let Some(content_changes) = params.get("contentChanges") {
                            if let Some(changes_array) = content_changes.as_array() {
                                // empty contentChanges チェック
                                if changes_array.is_empty() {
                                    tracing::debug!(
                                        uri = %url,
                                        "didChange received with empty contentChanges, ignoring"
                                    );
                                    return Ok(());
                                }

                                // full sync の場合、最後の change に全文がある
                                if let Some(last_change) = changes_array.last() {
                                    if let Some(new_text) = last_change.get("text").and_then(|t| t.as_str()) {
                                        // ドキュメントが存在する場合のみ更新
                                        if let Some(doc) = self.state.open_documents.get_mut(&url) {
                                            doc.text = new_text.to_string();

                                            // LSP の version を採用
                                            if let Some(v) = version {
                                                doc.version = v;
                                            }

                                            tracing::debug!(
                                                uri = %url,
                                                version = doc.version,
                                                text_len = new_text.len(),
                                                "Document text updated"
                                            );
                                        } else {
                                            tracing::warn!(
                                                uri = %url,
                                                "didChange for unopened document, ignoring"
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }
}
