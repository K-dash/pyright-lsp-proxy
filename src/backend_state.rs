use crate::backend::PyrightBackend;
use std::path::PathBuf;

/// Backend state
pub enum BackendState {
    /// Backend is running
    Running {
        backend: Box<PyrightBackend>,
        active_venv: PathBuf,
        session: u64,
    },
    /// Backend is disabled (venv not found)
    Disabled {
        reason: String,
        last_file: Option<PathBuf>,
    },
}

impl BackendState {
    /// Check if backend is in Disabled state
    pub fn is_disabled(&self) -> bool {
        matches!(self, BackendState::Disabled { .. })
    }

    /// Get active_venv (only when Running)
    pub fn active_venv(&self) -> Option<&PathBuf> {
        match self {
            BackendState::Running { active_venv, .. } => Some(active_venv),
            BackendState::Disabled { .. } => None,
        }
    }

    /// Get details of Disabled state
    pub fn disabled_info(&self) -> Option<(&str, Option<&PathBuf>)> {
        match self {
            BackendState::Disabled { reason, last_file } => {
                Some((reason.as_str(), last_file.as_ref()))
            }
            BackendState::Running { .. } => None,
        }
    }

    /// Get session when Running
    pub fn session(&self) -> Option<u64> {
        match self {
            BackendState::Running { session, .. } => Some(*session),
            BackendState::Disabled { .. } => None,
        }
    }
}
