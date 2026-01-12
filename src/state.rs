use crate::message::RpcMessage;
use std::path::PathBuf;
use url::Url;

/// プロキシが保持する状態（Phase 3b-1: 最小復元版）
pub struct ProxyState {
    /// 現在アクティブな .venv のパス
    pub active_venv: Option<PathBuf>,

    /// git toplevel（探索上限、初回取得でキャッシュ）
    pub git_toplevel: Option<PathBuf>,

    /// Claude Code からの initialize メッセージ（backend 初期化で流用）
    pub client_initialize: Option<RpcMessage>,

    /// 最後に開いたファイル（URI, text, venv）
    pub last_open: Option<(Url, String, PathBuf)>,

    /// backend 再起動の世代（ログと競合回避用）
    pub backend_session: u64,

    /// backend 切替中フラグ
    pub switching: bool,
}

impl ProxyState {
    pub fn new() -> Self {
        Self {
            active_venv: None,
            git_toplevel: None,
            client_initialize: None,
            last_open: None,
            backend_session: 0,
            switching: false,
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
