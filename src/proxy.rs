use crate::backend::PyrightBackend;
use crate::backend_state::BackendState;
use crate::error::ProxyError;
use crate::framing::{LspFrameReader, LspFrameWriter};
use crate::message::RpcMessage;
use crate::state::ProxyState;
use crate::venv;
use tokio::io::{stdin, stdout};

pub struct LspProxy {
    state: ProxyState,
}

impl LspProxy {
    pub fn new() -> Self {
        Self {
            state: ProxyState::new(),
        }
    }

    /// メインループ（Phase 3a: fallback env で即座に起動、Strict venv mode）
    pub async fn run(&mut self) -> Result<(), ProxyError> {
        // stdin/stdout のフレームリーダー/ライター
        let mut client_reader = LspFrameReader::new(stdin());
        let mut client_writer = LspFrameWriter::new(stdout());

        // 起動時 cwd を取得
        let cwd = std::env::current_dir()?;
        tracing::info!(cwd = %cwd.display(), "Starting pyright-lsp-proxy");

        // git toplevel を取得してキャッシュ
        self.state.git_toplevel = venv::get_git_toplevel(&cwd).await?;

        // fallback env を探索
        let fallback_venv = venv::find_fallback_venv(&cwd).await?;

        // backend を起動（fallback env で、なければ venv なし）
        let mut backend_state = if let Some(venv) = fallback_venv {
            tracing::info!(venv = %venv.display(), "Using fallback .venv");
            let backend = PyrightBackend::spawn(Some(&venv)).await?;
            BackendState::Running {
                backend: Box::new(backend),
                active_venv: venv,
                session: self.state.backend_session,
            }
        } else {
            tracing::warn!("No fallback .venv found, starting in Disabled mode (strict venv)");
            // venv なしで起動した場合は Disabled にする（strict mode）
            // backend は spawn しない（didOpen で venv が見つかったときに spawn する）
            BackendState::Disabled {
                reason: "No fallback .venv found".to_string(),
                last_file: None,
            }
        };

        let mut didopen_count = 0;

        loop {
            tokio::select! {
                // クライアント（Claude Code）からのメッセージ
                result = client_reader.read_message() => {
                    let msg = result?;
                    let method = msg.method_name();
                    let is_disabled = backend_state.is_disabled();

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

                        // Disabled 時も initialize には成功レスポンスを返す（capabilities は空）
                        if is_disabled {
                            tracing::warn!("Disabled mode: returning minimal initialize response");
                            let init_response = crate::message::RpcMessage {
                                jsonrpc: "2.0".to_string(),
                                id: msg.id.clone(),
                                method: None,
                                params: None,
                                result: Some(serde_json::json!({
                                    "capabilities": {}
                                })),
                                error: None,
                            };
                            client_writer.write_message(&init_response).await?;
                            continue;
                        }
                    }

                    // initialized notification（Disabled 時は無視）
                    if method == Some("initialized") && is_disabled {
                        tracing::debug!("Disabled mode: ignoring initialized notification");
                        continue;
                    }

                    // 1. didOpen は常に処理（復活トリガー）
                    if method == Some("textDocument/didOpen") {
                        didopen_count += 1;
                        self.handle_did_open(&msg, didopen_count, &mut backend_state, &mut client_writer).await?;
                        continue; // didOpen は handle 内で処理済み
                    }

                    // 2. didChange/didClose は常にキャッシュ更新
                    if method == Some("textDocument/didChange") {
                        self.handle_did_change(&msg).await?;
                        if is_disabled { continue; }  // Disabled時はbackendに送らない
                    }
                    if method == Some("textDocument/didClose") {
                        self.handle_did_close(&msg).await?;
                        if is_disabled { continue; }  // Disabled時はbackendに送らない
                    }

                    // 3. Request 処理（透過リトライ対応）
                    const VENV_CHECK_METHODS: &[&str] = &[
                        "textDocument/hover",
                        "textDocument/definition",
                        "textDocument/references",
                        "textDocument/documentSymbol",
                        "textDocument/typeDefinition",
                        "textDocument/implementation",
                    ];

                    if msg.is_request() {
                        let m = method;

                        // VENV_CHECK_METHODS なら ensure_backend_for_uri を通す
                        if let Some(method_name) = m {
                            if VENV_CHECK_METHODS.contains(&method_name) {
                                if let Some(url) = Self::extract_text_document_uri(&msg) {
                                    if let Ok(file_path) = url.to_file_path() {
                                        let old_session = backend_state.session();

                                        let switched = self
                                            .ensure_backend_for_uri(
                                                &mut backend_state,
                                                &mut client_writer,
                                                &url,
                                                &file_path,
                                            )
                                            .await?;

                                        if switched {
                                            tracing::info!(
                                                method = method_name,
                                                uri = %url,
                                                from_session = ?old_session,
                                                to_session = ?backend_state.session(),
                                                "Venv switched, request will be sent to new backend"
                                            );
                                        }
                                    } else {
                                        // URI 抽出成功したが file_path 変換失敗
                                        tracing::debug!(
                                            method = method_name,
                                            uri = %url,
                                            "Skipping venv check: could not convert URI to file path"
                                        );
                                    }
                                } else {
                                    // URI 抽出失敗 → 切替チェックをスキップ
                                    tracing::debug!(
                                        method = method_name,
                                        "Skipping venv check: could not extract textDocument.uri"
                                    );
                                }
                            }
                        }

                        // pending に登録（現在の backend session を記録）
                        if let Some(id) = &msg.id {
                            if let Some(session) = backend_state.session() {
                                self.state.pending_requests.insert(
                                    id.clone(),
                                    crate::state::PendingRequest {
                                        backend_session: session,
                                    },
                                );
                            }
                        }
                    }

                    // 4. Disabled 時はリクエストにエラーを返す（VENV_CHECK_METHODS 以外）
                    // ★ ensure_backend_for_uri() で状態が変わった可能性があるので再取得
                    let is_disabled = backend_state.is_disabled();
                    if is_disabled && msg.is_request() {
                        let error_message = "pyright-lsp-proxy: .venv not found (strict mode). Create .venv or run hooks.";
                        if let Some((reason, last_file)) = backend_state.disabled_info() {
                            tracing::warn!(
                                method = ?method,
                                reason = reason,
                                last_file = ?last_file.map(|p| p.display().to_string()),
                                error_message = error_message,
                                "Returning error response to client (Disabled mode)"
                            );
                        }
                        let error_response = create_error_response(&msg, error_message);
                        client_writer.write_message(&error_response).await?;
                        continue;
                    }

                    // 5. Running 時は backend に転送
                    if let BackendState::Running { backend, session, .. } = &mut backend_state {
                        if msg.is_request() {
                            tracing::debug!(
                                session = *session,
                                method = msg.method.as_deref(),
                                "Sending request to backend"
                            );
                        }
                        backend.send_message(&msg).await?;
                    }
                }

                // Running 時のみ backend からの読み取りを待つ
                result = async {
                    match &mut backend_state {
                        BackendState::Running { backend, .. } => backend.read_message().await,
                        BackendState::Disabled { .. } => std::future::pending().await,
                    }
                } => {
                    let msg = result?;
                    let running_session = backend_state.session();

                    tracing::debug!(
                        is_response = msg.is_response(),
                        is_notification = msg.is_notification(),
                        "Backend -> Proxy"
                    );

                    // backend からのレスポンスで pending を解決 + 世代チェック
                    if msg.is_response() {
                        if let Some(id) = &msg.id {
                            // pending から取得して世代チェック
                            if let Some(pending) = self.state.pending_requests.get(id) {
                                if Some(pending.backend_session) != running_session {
                                    // 古い世代からの response → 破棄
                                    tracing::warn!(
                                        id = ?id,
                                        pending_session = pending.backend_session,
                                        running_session = ?running_session,
                                        "Discarding stale response from old backend session"
                                    );
                                    self.state.pending_requests.remove(id);
                                    continue;
                                }
                            }
                            self.state.pending_requests.remove(id);
                        }
                    }

                    // クライアントに転送
                    client_writer.write_message(&msg).await?;
                }
            }
        }
    }

    /// LSP request の params から textDocument.uri を抽出
    fn extract_text_document_uri(msg: &RpcMessage) -> Option<url::Url> {
        let params = msg.params.as_ref()?;
        let text_document = params.get("textDocument")?;
        let uri_str = text_document.get("uri")?.as_str()?;
        url::Url::parse(uri_str).ok()
    }

    /// URI に基づいて適切な backend を確保する
    /// 戻り値: 切り替えが発生したか
    async fn ensure_backend_for_uri(
        &mut self,
        backend_state: &mut BackendState,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
        url: &url::Url,
        file_path: &std::path::Path,
    ) -> Result<bool, ProxyError> {
        // キャッシュから venv を取得（O(1)）
        let target_venv = if let Some(doc) = self.state.open_documents.get(url) {
            doc.venv.clone()
        } else {
            // didOpen が来ていない URI → 探索（例外経路）
            tracing::debug!(uri = %url, "URI not in cache, searching venv");
            venv::find_venv(file_path, self.state.git_toplevel.as_deref()).await?
        };

        // 共通関数で状態遷移
        self.transition_backend_state(
            backend_state,
            client_writer,
            target_venv.as_deref(),
            file_path,
        )
        .await
    }

    /// venv に基づいて backend の状態を遷移させる
    /// 戻り値: 切り替えが発生したかどうか
    async fn transition_backend_state(
        &mut self,
        backend_state: &mut BackendState,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
        target_venv: Option<&std::path::Path>,
        trigger_file: &std::path::Path,
    ) -> Result<bool, ProxyError> {
        match (&*backend_state, target_venv) {
            // Running + same venv → 何もしない
            (BackendState::Running { active_venv, .. }, Some(venv)) if active_venv == venv => {
                tracing::debug!(venv = %venv.display(), "Using same .venv as before");
                Ok(false)
            }

            // Running + different venv → 切替
            (BackendState::Running { .. }, Some(venv)) => {
                let old_session = backend_state.session().unwrap_or(0);

                tracing::warn!(
                    current = ?backend_state.active_venv().map(|p| p.display().to_string()),
                    found = %venv.display(),
                    "Venv switch needed, restarting backend"
                );

                if let BackendState::Running { backend, .. } = backend_state {
                    // 旧 session の pending をキャンセル
                    self.cancel_pending_requests_for_session(client_writer, old_session)
                        .await?;

                    let new_backend = self
                        .restart_backend_with_venv(backend, venv, client_writer)
                        .await?;
                    *backend_state = BackendState::Running {
                        backend: Box::new(new_backend),
                        active_venv: venv.to_path_buf(),
                        session: self.state.backend_session,
                    };
                }
                Ok(true)
            }

            // Running + venv not found → Disabled へ
            (BackendState::Running { .. }, None) => {
                tracing::warn!(
                    path = %trigger_file.display(),
                    "No .venv found for this file, disabling backend"
                );

                if let BackendState::Running { backend, .. } = backend_state {
                    self.disable_backend(backend, client_writer, trigger_file)
                        .await?;
                    *backend_state = BackendState::Disabled {
                        reason: format!("No .venv found for file: {}", trigger_file.display()),
                        last_file: Some(trigger_file.to_path_buf()),
                    };
                }
                Ok(true)
            }

            // Disabled + venv found → Running へ復活
            (BackendState::Disabled { .. }, Some(venv)) => {
                tracing::info!(venv = %venv.display(), "Found .venv, spawning backend");
                let new_backend = self.spawn_and_init_backend(venv, client_writer).await?;
                *backend_state = BackendState::Running {
                    backend: Box::new(new_backend),
                    active_venv: venv.to_path_buf(),
                    session: self.state.backend_session,
                };
                Ok(true)
            }

            // Disabled + venv not found → そのまま
            (BackendState::Disabled { reason, last_file }, None) => {
                tracing::warn!(
                    path = %trigger_file.display(),
                    reason = reason.as_str(),
                    last_file = ?last_file.as_ref().map(|p| p.display().to_string()),
                    "No .venv found for this file (backend still disabled)"
                );
                Ok(false)
            }
        }
    }

    /// didOpen 処理 & .venv 切替判定 + BackendState 遷移（Strict venv mode）
    async fn handle_did_open(
        &mut self,
        msg: &crate::message::RpcMessage,
        count: usize,
        backend_state: &mut BackendState,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<(), ProxyError> {
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
                                    .unwrap_or(0)
                                    as i32;

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

                                // .venv 探索
                                let found_venv =
                                    venv::find_venv(&file_path, self.state.git_toplevel.as_deref())
                                        .await?;

                                // didOpen をキャッシュ（Disabled時の復活用）
                                if let Some(text_content) = &text {
                                    let doc = crate::state::OpenDocument {
                                        language_id: language_id.clone(),
                                        version,
                                        text: text_content.clone(),
                                        venv: found_venv.clone(),
                                    };
                                    self.state.open_documents.insert(url.clone(), doc);
                                    tracing::debug!(
                                        uri = %url,
                                        doc_count = self.state.open_documents.len(),
                                        cached_venv = ?found_venv.as_ref().map(|p| p.display().to_string()),
                                        "Document cached"
                                    );
                                }

                                // 状態遷移ロジック（Strict venv mode）
                                tracing::debug!(
                                    is_running = !backend_state.is_disabled(),
                                    is_disabled = backend_state.is_disabled(),
                                    has_venv = found_venv.is_some(),
                                    "State transition check"
                                );

                                self.transition_backend_state(
                                    backend_state,
                                    client_writer,
                                    found_venv.as_deref(),
                                    &file_path,
                                )
                                .await?;
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// backend を graceful shutdown して新しい .venv で再起動（Phase 3b-1）
    async fn restart_backend_with_venv(
        &mut self,
        backend: &mut PyrightBackend,
        new_venv: &std::path::Path,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<PyrightBackend, ProxyError> {
        self.state.backend_session += 1;
        let session = self.state.backend_session;

        tracing::info!(
            session = session,
            new_venv = %new_venv.display(),
            "Starting backend restart sequence"
        );

        // 1. 既存 backend を shutdown
        if let Err(e) = backend.shutdown_gracefully().await {
            tracing::error!(error = ?e, "Failed to shutdown backend gracefully");
            // エラーでも続行（新 backend 起動を試みる）
        }

        // 2. 新しい backend を起動
        tracing::info!(session = session, venv = %new_venv.display(), "Spawning new backend");
        let mut new_backend = PyrightBackend::spawn(Some(new_venv)).await?;

        // 3. backend に initialize を送る（プロキシが backend クライアントになる）
        let init_params = self
            .state
            .client_initialize
            .as_ref()
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
                    crate::error::BackendError::InitializeTimeout(10),
                ));
            }

            let wait_result = tokio::time::timeout(remaining, new_backend.read_message()).await;

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
                                            format!(
                                                "code={}, message={}",
                                                error.code, error.message
                                            ),
                                        ),
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
                        crate::error::BackendError::InitializeFailed(format!(
                            "Error reading initialize response: {}",
                            e
                        )),
                    ));
                }
                Err(_) => {
                    return Err(ProxyError::Backend(
                        crate::error::BackendError::InitializeTimeout(10),
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

        // 6. ドキュメント復元（Phase 3b-2）
        // 新しい venv の親ディレクトリ配下にあるドキュメントのみを復元
        let venv_parent = new_venv.parent().map(|p| p.to_path_buf());
        let total_docs = self.state.open_documents.len();
        let mut restored = 0;
        let mut skipped = 0;
        let mut failed = 0;
        let mut skipped_uris: Vec<url::Url> = Vec::new();

        tracing::info!(
            session = session,
            total_docs = total_docs,
            venv_parent = ?venv_parent.as_ref().map(|p| p.display().to_string()),
            "Starting document restoration"
        );

        for (url, doc) in &self.state.open_documents {
            // venv の親ディレクトリ配下にあるドキュメントのみを復元
            let should_restore = match (url.to_file_path().ok(), &venv_parent) {
                (Some(file_path), Some(venv_parent)) => file_path.starts_with(venv_parent),
                _ => false, // file:// URL でない、または venv_parent がない場合はスキップ
            };

            if !should_restore {
                skipped += 1;
                skipped_uris.push(url.clone());
                tracing::debug!(
                    session = session,
                    uri = %url,
                    "Skipping document from different venv"
                );
                continue;
            }
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
            skipped = skipped,
            failed = failed,
            total = total_docs,
            "Document restoration completed"
        );

        // スキップしたURIのdiagnosticsをクリア
        if !skipped_uris.is_empty() {
            let (ok, clear_failed) = self.clear_diagnostics_for_uris(&skipped_uris, client_writer).await;

            if clear_failed == 0 {
                tracing::info!(
                    session = session,
                    cleared_ok = ok,
                    "Diagnostics cleared for skipped documents"
                );
            } else {
                tracing::info!(
                    session = session,
                    cleared_ok = ok,
                    cleared_failed = clear_failed,
                    "Diagnostics clear partially failed for skipped documents"
                );
            }
        }

        tracing::info!(
            session = session,
            venv = %new_venv.display(),
            "Backend restart completed successfully"
        );

        Ok(new_backend)
    }

    /// 指定URIのdiagnosticsをクリア（空配列を送信）
    /// ベストエフォート: 1件失敗しても続行
    async fn clear_diagnostics_for_uris(
        &self,
        uris: &[url::Url],
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> (usize, usize) {
        let mut ok = 0;
        let mut failed = 0;

        for uri in uris {
            tracing::trace!(uri = %uri, "Clearing diagnostics");

            let clear_msg = RpcMessage {
                jsonrpc: "2.0".to_string(),
                id: None,
                method: Some("textDocument/publishDiagnostics".to_string()),
                params: Some(serde_json::json!({
                    "uri": uri.to_string(),
                    "diagnostics": []
                })),
                result: None,
                error: None,
            };

            match client_writer.write_message(&clear_msg).await {
                Ok(_) => ok += 1,
                Err(e) => {
                    failed += 1;
                    tracing::warn!(uri = %uri, error = ?e, "Failed to clear diagnostics");
                }
            }
        }

        (ok, failed)
    }

    /// backend を shutdown して Disabled 状態へ（Strict venv mode）
    async fn disable_backend(
        &mut self,
        backend: &mut PyrightBackend,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
        file_path: &std::path::Path,
    ) -> Result<(), ProxyError> {
        self.state.backend_session += 1;
        let session = self.state.backend_session;

        tracing::info!(
            session = session,
            file = %file_path.display(),
            "Disabling backend (no .venv found)"
        );

        // open_documents の全URIへ空diagnosticsを送信（借用地雷回避: 先にclone）
        let uris: Vec<url::Url> = self.state.open_documents.keys().cloned().collect();
        let (ok, failed) = self.clear_diagnostics_for_uris(&uris, client_writer).await;

        if failed == 0 {
            tracing::info!(
                session = session,
                cleared_ok = ok,
                "Diagnostics cleared for all open documents"
            );
        } else {
            tracing::info!(
                session = session,
                cleared_ok = ok,
                cleared_failed = failed,
                "Diagnostics clear partially failed"
            );
        }

        // 未解決リクエストへ RequestCancelled を返す
        self.cancel_pending_requests(client_writer).await?;

        // backend を shutdown
        if let Err(e) = backend.shutdown_gracefully().await {
            tracing::error!(error = ?e, "Failed to shutdown backend gracefully");
        }

        tracing::info!(session = session, "Backend disabled");

        Ok(())
    }

    /// backend を spawn して initialize する（Disabled → Running 復活用）
    async fn spawn_and_init_backend(
        &mut self,
        venv: &std::path::Path,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<PyrightBackend, ProxyError> {
        self.state.backend_session += 1;
        let session = self.state.backend_session;

        tracing::info!(
            session = session,
            venv = %venv.display(),
            "Spawning backend from Disabled state"
        );

        // 1. 新しい backend を起動
        let mut new_backend = PyrightBackend::spawn(Some(venv)).await?;

        // 2. backend に initialize を送る
        let init_params = self
            .state
            .client_initialize
            .as_ref()
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

        // 3. initialize response を受信
        let init_id = 1i64;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(ProxyError::Backend(
                    crate::error::BackendError::InitializeTimeout(10),
                ));
            }

            let wait_result = tokio::time::timeout(remaining, new_backend.read_message()).await;

            match wait_result {
                Ok(Ok(msg)) => {
                    if msg.is_response() {
                        if let Some(crate::message::RpcId::Number(id)) = &msg.id {
                            if *id == init_id {
                                if let Some(error) = &msg.error {
                                    return Err(ProxyError::Backend(
                                        crate::error::BackendError::InitializeResponseError(
                                            format!("code={}, message={}", error.code, error.message),
                                        ),
                                    ));
                                }

                                tracing::info!(
                                    session = session,
                                    response_id = ?msg.id,
                                    "Received initialize response from backend"
                                );

                                break;
                            }
                        }
                    } else {
                        tracing::debug!(
                            session = session,
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
                        crate::error::BackendError::InitializeTimeout(10),
                    ));
                }
            }
        }

        // 4. initialized notification を送る
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

        // 5. ドキュメント復元（venv の親ディレクトリ配下のみ）
        let venv_parent = venv.parent().map(|p| p.to_path_buf());
        let total_docs = self.state.open_documents.len();
        let mut restored = 0;
        let mut skipped = 0;
        let mut failed = 0;
        let mut skipped_uris: Vec<url::Url> = Vec::new();

        tracing::info!(
            session = session,
            total_docs = total_docs,
            venv_parent = ?venv_parent.as_ref().map(|p| p.display().to_string()),
            "Starting document restoration"
        );

        for (url, doc) in &self.state.open_documents {
            let should_restore = match (url.to_file_path().ok(), &venv_parent) {
                (Some(file_path), Some(venv_parent)) => file_path.starts_with(venv_parent),
                _ => false,
            };

            if !should_restore {
                skipped += 1;
                skipped_uris.push(url.clone());
                tracing::debug!(
                    session = session,
                    uri = %url,
                    "Skipping document from different venv"
                );
                continue;
            }

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
                }
            }
        }

        tracing::info!(
            session = session,
            restored = restored,
            skipped = skipped,
            failed = failed,
            total = total_docs,
            "Document restoration completed"
        );

        // スキップしたURIのdiagnosticsをクリア
        if !skipped_uris.is_empty() {
            let (ok, clear_failed) = self.clear_diagnostics_for_uris(&skipped_uris, client_writer).await;

            if clear_failed == 0 {
                tracing::info!(
                    session = session,
                    cleared_ok = ok,
                    "Diagnostics cleared for skipped documents"
                );
            } else {
                tracing::info!(
                    session = session,
                    cleared_ok = ok,
                    cleared_failed = clear_failed,
                    "Diagnostics clear partially failed for skipped documents"
                );
            }
        }

        tracing::info!(
            session = session,
            venv = %venv.display(),
            "Backend spawned and initialized successfully"
        );

        Ok(new_backend)
    }

    /// 未解決リクエストに RequestCancelled を返す
    async fn cancel_pending_requests(
        &mut self,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
    ) -> Result<(), ProxyError> {
        const REQUEST_CANCELLED: i64 = -32800;
        let pending: Vec<_> = self
            .state
            .pending_requests
            .drain()
            .map(|(id, _)| id)
            .collect();

        for id in pending {
            let msg = crate::message::RpcMessage {
                jsonrpc: "2.0".to_string(),
                id: Some(id),
                method: None,
                params: None,
                result: None,
                error: Some(crate::message::RpcError {
                    code: REQUEST_CANCELLED,
                    message: "Request cancelled".to_string(),
                    data: None,
                }),
            };

            client_writer.write_message(&msg).await?;
        }

        Ok(())
    }

    /// 指定した session の pending request をキャンセル
    async fn cancel_pending_requests_for_session(
        &mut self,
        client_writer: &mut LspFrameWriter<tokio::io::Stdout>,
        old_session: u64,
    ) -> Result<(), ProxyError> {
        const REQUEST_CANCELLED: i64 = -32800;

        let to_cancel: Vec<_> = self
            .state
            .pending_requests
            .iter()
            .filter(|(_, p)| p.backend_session == old_session)
            .map(|(id, _)| id.clone())
            .collect();

        for id in to_cancel {
            self.state.pending_requests.remove(&id);
            let msg = crate::message::RpcMessage {
                jsonrpc: "2.0".to_string(),
                id: Some(id.clone()),
                method: None,
                params: None,
                result: None,
                error: Some(crate::message::RpcError {
                    code: REQUEST_CANCELLED,
                    message: "Request cancelled due to backend restart".to_string(),
                    data: None,
                }),
            };
            client_writer.write_message(&msg).await?;
            tracing::info!(id = ?id, session = old_session, "Cancelled pending request");
        }

        Ok(())
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

                        // contentChanges から text を取得
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

                                // ドキュメントが存在する場合のみ更新
                                if let Some(doc) = self.state.open_documents.get_mut(&url) {
                                    // 各変更を順番に適用
                                    for change in changes_array {
                                        if let Some(range) = change.get("range") {
                                            // Incremental sync: range を使って部分更新
                                            if let Some(new_text) =
                                                change.get("text").and_then(|t| t.as_str())
                                            {
                                                Self::apply_incremental_change(
                                                    &mut doc.text,
                                                    range,
                                                    new_text,
                                                )?;
                                                tracing::debug!(
                                                    uri = %url,
                                                    "Applied incremental change"
                                                );
                                            }
                                        } else {
                                            // Full sync: 全文置換
                                            if let Some(new_text) =
                                                change.get("text").and_then(|t| t.as_str())
                                            {
                                                doc.text = new_text.to_string();
                                                tracing::debug!(
                                                    uri = %url,
                                                    text_len = new_text.len(),
                                                    "Applied full sync change"
                                                );
                                            }
                                        }
                                    }

                                    // LSP の version を採用
                                    if let Some(v) = version {
                                        doc.version = v;
                                    }

                                    tracing::debug!(
                                        uri = %url,
                                        version = doc.version,
                                        text_len = doc.text.len(),
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

        Ok(())
    }

    /// didClose 処理：キャッシュからドキュメントを削除
    async fn handle_did_close(
        &mut self,
        msg: &crate::message::RpcMessage,
    ) -> Result<(), ProxyError> {
        if let Some(params) = &msg.params {
            if let Some(text_document) = params.get("textDocument") {
                if let Some(uri_str) = text_document.get("uri").and_then(|u| u.as_str()) {
                    if let Ok(url) = url::Url::parse(uri_str) {
                        if self.state.open_documents.remove(&url).is_some() {
                            tracing::debug!(
                                uri = %url,
                                remaining_docs = self.state.open_documents.len(),
                                "Document removed from cache"
                            );
                        } else {
                            tracing::warn!(
                                uri = %url,
                                "didClose for unknown document"
                            );
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Incremental change を適用（range ベースの部分置換）
    fn apply_incremental_change(
        text: &mut String,
        range: &serde_json::Value,
        new_text: &str,
    ) -> Result<(), ProxyError> {
        // range から start/end を取得
        let start = range.get("start").ok_or_else(|| {
            ProxyError::InvalidMessage("didChange range missing start".to_string())
        })?;
        let end = range
            .get("end")
            .ok_or_else(|| ProxyError::InvalidMessage("didChange range missing end".to_string()))?;

        let start_line =
            start.get("line").and_then(|l| l.as_u64()).ok_or_else(|| {
                ProxyError::InvalidMessage("didChange start missing line".to_string())
            })? as usize;
        let start_char = start
            .get("character")
            .and_then(|c| c.as_u64())
            .ok_or_else(|| {
                ProxyError::InvalidMessage("didChange start missing character".to_string())
            })? as usize;

        let end_line =
            end.get("line").and_then(|l| l.as_u64()).ok_or_else(|| {
                ProxyError::InvalidMessage("didChange end missing line".to_string())
            })? as usize;
        let end_char = end
            .get("character")
            .and_then(|c| c.as_u64())
            .ok_or_else(|| {
                ProxyError::InvalidMessage("didChange end missing character".to_string())
            })? as usize;

        // line/character を byte offset に変換
        let start_offset = Self::position_to_offset(text, start_line, start_char)?;
        let end_offset = Self::position_to_offset(text, end_line, end_char)?;

        // 範囲の検証（start > end は不正）
        if start_offset > end_offset {
            return Err(ProxyError::InvalidMessage(format!(
                "Invalid range: start offset ({}) > end offset ({})",
                start_offset, end_offset
            )));
        }

        // 範囲を置換
        text.replace_range(start_offset..end_offset, new_text);

        Ok(())
    }

    /// LSP position (line, character) を byte offset に変換
    /// LSP の character は UTF-16 code unit 数
    fn position_to_offset(text: &str, line: usize, character: usize) -> Result<usize, ProxyError> {
        let mut current_line = 0;
        let mut line_start_offset = 0;

        for (idx, ch) in text.char_indices() {
            if ch == '\n' {
                if current_line == line {
                    // 目的の行の終端に到達（改行文字の前）
                    return Self::find_offset_in_line(text, line_start_offset, idx, character);
                }
                current_line += 1;
                line_start_offset = idx + 1;
            }
        }

        // 最終行（改行で終わらない場合）または空テキストの最初の行
        if current_line == line {
            return Self::find_offset_in_line(text, line_start_offset, text.len(), character);
        }

        // 行番号が範囲外
        Err(ProxyError::InvalidMessage(format!(
            "Position out of range: line={} (max={}), character={}",
            line, current_line, character
        )))
    }

    /// 行内で UTF-16 code unit をカウントして byte offset を返す
    /// character が行長を超える場合は行末に clamp
    fn find_offset_in_line(
        text: &str,
        line_start: usize,
        line_end: usize,
        character: usize,
    ) -> Result<usize, ProxyError> {
        let line_text = &text[line_start..line_end];
        let mut utf16_offset = 0;

        for (idx, ch) in line_text.char_indices() {
            if utf16_offset >= character {
                return Ok(line_start + idx);
            }
            utf16_offset += ch.len_utf16();
        }

        // character が行長を超える場合は行末に clamp
        Ok(line_end)
    }
}

/// エラーレスポンスを作成（Disabled 時のリクエストに返す）
fn create_error_response(request: &RpcMessage, message: &str) -> RpcMessage {
    RpcMessage {
        jsonrpc: "2.0".to_string(),
        id: request.id.clone(),
        method: None,
        params: None,
        result: None,
        error: Some(crate::message::RpcError {
            code: -32603,  // Internal error (互換性のため)
            message: message.to_string(),
            data: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_position_to_offset_simple() {
        let text = "hello\nworld\n";

        // line 0, char 0 -> offset 0
        assert_eq!(LspProxy::position_to_offset(text, 0, 0).unwrap(), 0);

        // line 0, char 5 -> offset 5 (end of "hello")
        assert_eq!(LspProxy::position_to_offset(text, 0, 5).unwrap(), 5);

        // line 1, char 0 -> offset 6 (start of "world")
        assert_eq!(LspProxy::position_to_offset(text, 1, 0).unwrap(), 6);

        // line 1, char 5 -> offset 11 (end of "world")
        assert_eq!(LspProxy::position_to_offset(text, 1, 5).unwrap(), 11);
    }

    #[test]
    fn test_position_to_offset_multibyte() {
        // マルチバイト文字を含むテキスト
        let text = "こんにちは\nworld\n";

        // line 0, char 0 -> offset 0
        assert_eq!(LspProxy::position_to_offset(text, 0, 0).unwrap(), 0);

        // line 0, char 1 -> offset 3 (after "こ")
        assert_eq!(LspProxy::position_to_offset(text, 0, 1).unwrap(), 3);

        // line 1, char 0 -> offset 16 (start of "world", after "こんにちは\n")
        assert_eq!(LspProxy::position_to_offset(text, 1, 0).unwrap(), 16);
    }

    #[test]
    fn test_apply_incremental_change_simple_replace() {
        let mut text = "hello world".to_string();
        let range = json!({
            "start": { "line": 0, "character": 0 },
            "end": { "line": 0, "character": 5 }
        });

        LspProxy::apply_incremental_change(&mut text, &range, "hi").unwrap();
        assert_eq!(text, "hi world");
    }

    #[test]
    fn test_apply_incremental_change_insert() {
        let mut text = "hello world".to_string();
        let range = json!({
            "start": { "line": 0, "character": 5 },
            "end": { "line": 0, "character": 5 }
        });

        // 挿入（range が空）
        LspProxy::apply_incremental_change(&mut text, &range, " beautiful").unwrap();
        assert_eq!(text, "hello beautiful world");
    }

    #[test]
    fn test_apply_incremental_change_delete() {
        let mut text = "hello beautiful world".to_string();
        let range = json!({
            "start": { "line": 0, "character": 5 },
            "end": { "line": 0, "character": 15 }
        });

        // 削除（new_text が空）
        LspProxy::apply_incremental_change(&mut text, &range, "").unwrap();
        assert_eq!(text, "hello world");
    }

    #[test]
    fn test_apply_incremental_change_multiline() {
        let mut text = "def hello():\n    print('hello')\n".to_string();
        let range = json!({
            "start": { "line": 1, "character": 11 },
            "end": { "line": 1, "character": 16 }
        });

        // "hello" を "world" に置換
        LspProxy::apply_incremental_change(&mut text, &range, "world").unwrap();
        assert_eq!(text, "def hello():\n    print('world')\n");
    }

    #[test]
    fn test_apply_incremental_change_cross_line() {
        let mut text = "line1\nline2\nline3\n".to_string();
        let range = json!({
            "start": { "line": 0, "character": 5 },
            "end": { "line": 2, "character": 0 }
        });

        // 複数行にまたがる削除
        LspProxy::apply_incremental_change(&mut text, &range, "").unwrap();
        assert_eq!(text, "line1line3\n");
    }

    #[test]
    fn test_position_to_offset_surrogate_pair() {
        // サロゲートペア（絵文字）を含むテキスト
        // 😀 は U+1F600 で UTF-16 では 2 code units (サロゲートペア)
        // UTF-8 では 4 bytes
        let text = "a😀b\n";

        // line 0, char 0 -> offset 0 (before 'a')
        assert_eq!(LspProxy::position_to_offset(text, 0, 0).unwrap(), 0);

        // line 0, char 1 -> offset 1 (before '😀')
        assert_eq!(LspProxy::position_to_offset(text, 0, 1).unwrap(), 1);

        // line 0, char 3 -> offset 5 (before 'b', 😀 は UTF-16 で 2 code units)
        assert_eq!(LspProxy::position_to_offset(text, 0, 3).unwrap(), 5);

        // line 0, char 4 -> offset 6 (before '\n')
        assert_eq!(LspProxy::position_to_offset(text, 0, 4).unwrap(), 6);
    }

    #[test]
    fn test_position_to_offset_line_end_clamp() {
        // 行末を超える character は行末に clamp される
        let text = "abc\ndef\n";

        // line 0, char 100 -> offset 3 (行末に clamp)
        assert_eq!(LspProxy::position_to_offset(text, 0, 100).unwrap(), 3);

        // line 1, char 100 -> offset 7 (行末に clamp)
        assert_eq!(LspProxy::position_to_offset(text, 1, 100).unwrap(), 7);
    }

    #[test]
    fn test_position_to_offset_line_out_of_range() {
        let text = "abc\ndef\n";

        // line 10 は範囲外
        let result = LspProxy::position_to_offset(text, 10, 0);
        assert!(result.is_err());
    }

    #[test]
    fn test_apply_incremental_change_invalid_range() {
        // start > end の不正な範囲
        let mut text = "hello world".to_string();
        let range = json!({
            "start": { "line": 0, "character": 10 },
            "end": { "line": 0, "character": 5 }
        });

        let result = LspProxy::apply_incremental_change(&mut text, &range, "test");
        assert!(result.is_err());
    }

    #[test]
    fn test_apply_incremental_change_with_emoji() {
        // 絵文字を含むテキストの編集
        let mut text = "hello 😀 world".to_string();
        // "😀 " を削除 (position 6 から 9: 😀 は UTF-16 で 2 code units + space 1)
        let range = json!({
            "start": { "line": 0, "character": 6 },
            "end": { "line": 0, "character": 9 }
        });

        LspProxy::apply_incremental_change(&mut text, &range, "").unwrap();
        assert_eq!(text, "hello world");
    }

    #[test]
    fn test_position_to_offset_empty_text() {
        let text = "";

        // 空テキストでも line 0, char 0 は有効
        assert_eq!(LspProxy::position_to_offset(text, 0, 0).unwrap(), 0);
    }

    #[test]
    fn test_position_to_offset_no_trailing_newline() {
        // 末尾に改行がないテキスト
        let text = "abc";

        assert_eq!(LspProxy::position_to_offset(text, 0, 0).unwrap(), 0);
        assert_eq!(LspProxy::position_to_offset(text, 0, 3).unwrap(), 3);
    }
}
