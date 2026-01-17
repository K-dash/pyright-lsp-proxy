use crate::backend::PyrightBackend;
use std::path::PathBuf;

/// backend の状態
pub enum BackendState {
    /// backend が動作中
    Running {
        backend: Box<PyrightBackend>,
        active_venv: PathBuf,
    },
    /// backend が無効（venv が見つからない）
    Disabled {
        reason: String,
        last_file: Option<PathBuf>,
    },
}

impl BackendState {
    /// backend が Disabled 状態かどうか
    pub fn is_disabled(&self) -> bool {
        matches!(self, BackendState::Disabled { .. })
    }

    /// active_venv を取得（Running 時のみ）
    pub fn active_venv(&self) -> Option<&PathBuf> {
        match self {
            BackendState::Running { active_venv, .. } => Some(active_venv),
            BackendState::Disabled { .. } => None,
        }
    }

    /// Disabled 状態の詳細を取得
    pub fn disabled_info(&self) -> Option<(&str, Option<&PathBuf>)> {
        match self {
            BackendState::Disabled { reason, last_file } => {
                Some((reason.as_str(), last_file.as_ref()))
            }
            BackendState::Running { .. } => None,
        }
    }
}
