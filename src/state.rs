use crate::message::RpcMessage;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use url::Url;

/// 開いているドキュメント（Phase 3b-2）
#[derive(Debug, Clone)]
pub struct OpenDocument {
    pub language_id: String,
    pub version: i32,
    pub text: String,
}

/// プロキシが保持する状態（Phase 3b-2: 複数ドキュメント復元版）
pub struct ProxyState {
    /// 現在アクティブな .venv のパス
    pub active_venv: Option<PathBuf>,

    /// git toplevel（探索上限、初回取得でキャッシュ）
    pub git_toplevel: Option<PathBuf>,

    /// Claude Code からの initialize メッセージ（backend 初期化で流用）
    pub client_initialize: Option<RpcMessage>,

    /// 開いているドキュメント（Phase 3b-2）
    pub open_documents: HashMap<Url, OpenDocument>,

    /// backend 再起動の世代（ログと競合回避用）
    pub backend_session: u64,

    /// 未解決リクエストの ID（再起動時のキャンセル通知用）
    pub pending_requests: HashSet<crate::message::RpcId>,
}

impl ProxyState {
    pub fn new() -> Self {
        Self {
            active_venv: None,
            git_toplevel: None,
            client_initialize: None,
            open_documents: HashMap::new(),
            backend_session: 0,
            pending_requests: HashSet::new(),
        }
    }

    /// .venv 切替が必要かどうか判定
    pub fn needs_venv_switch(&self, new_venv: &PathBuf) -> bool {
        match &self.active_venv {
            Some(current) => current != new_venv,
            None => true,
        }
    }
}

impl Default for ProxyState {
    fn default() -> Self {
        Self::new()
    }
}
