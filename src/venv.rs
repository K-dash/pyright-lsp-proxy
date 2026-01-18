use crate::error::VenvError;
use std::path::{Path, PathBuf};
use tokio::process::Command;

const VENV_DIR: &str = ".venv";
const PYVENV_CFG: &str = "pyvenv.cfg";

/// Execute git rev-parse --show-toplevel and get result
pub async fn get_git_toplevel(working_dir: &Path) -> Result<Option<PathBuf>, VenvError> {
    let output = match Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(working_dir)
        .output()
        .await
    {
        Ok(output) => output,
        Err(e) => {
            tracing::warn!(error = ?e, "git command failed (git not installed or not executable), continuing without git");
            return Ok(None);
        }
    };

    if output.status.success() {
        let path_str = String::from_utf8_lossy(&output.stdout);
        let path = PathBuf::from(path_str.trim());
        tracing::info!(toplevel = %path.display(), "Git toplevel found");
        Ok(Some(path))
    } else {
        tracing::warn!("Not in a git repository");
        Ok(None)
    }
}

/// Search for .venv by traversing parent directories from file path
///
/// # Arguments
/// * `file_path` - Starting file path
/// * `git_toplevel` - Search boundary (if None, search up to root)
pub async fn find_venv(
    file_path: &Path,
    git_toplevel: Option<&Path>,
) -> Result<Option<PathBuf>, VenvError> {
    tracing::debug!(
        file = %file_path.display(),
        toplevel = ?git_toplevel.map(|p| p.display().to_string()),
        "Starting .venv search"
    );

    // Start from file's parent directory
    let mut current = file_path.parent();
    let mut depth = 0;

    while let Some(dir) = current {
        tracing::trace!(
            depth = depth,
            dir = %dir.display(),
            "Searching for .venv"
        );

        // Stop if we exceed git toplevel
        if let Some(toplevel) = git_toplevel {
            if !dir.starts_with(toplevel) {
                tracing::debug!(
                    dir = %dir.display(),
                    toplevel = %toplevel.display(),
                    "Reached git toplevel boundary"
                );
                break;
            }
        }

        // Check for .venv/pyvenv.cfg existence
        let venv_path = dir.join(VENV_DIR);
        let pyvenv_cfg = venv_path.join(PYVENV_CFG);

        if pyvenv_cfg.exists() {
            tracing::info!(
                venv = %venv_path.display(),
                depth = depth,
                ".venv found"
            );
            return Ok(Some(venv_path));
        }

        // Move to parent directory
        current = dir.parent();
        depth += 1;
    }

    tracing::warn!(
        file = %file_path.display(),
        depth = depth,
        "No .venv found"
    );
    Ok(None)
}

/// Search for fallback env (.venv search from cwd at startup)
pub async fn find_fallback_venv(cwd: &Path) -> Result<Option<PathBuf>, VenvError> {
    tracing::info!(cwd = %cwd.display(), "Searching for fallback .venv");

    // 1. Get git toplevel
    let git_toplevel = get_git_toplevel(cwd).await?;

    // 2. Search for .venv from toplevel
    if let Some(toplevel) = &git_toplevel {
        let venv_path = toplevel.join(VENV_DIR);
        let pyvenv_cfg = venv_path.join(PYVENV_CFG);

        tracing::debug!(
            toplevel = %toplevel.display(),
            checking_path = %venv_path.display(),
            pyvenv_cfg = %pyvenv_cfg.display(),
            exists = pyvenv_cfg.exists(),
            "Checking git toplevel for .venv"
        );

        if pyvenv_cfg.exists() {
            tracing::info!(
                venv = %venv_path.display(),
                "Fallback .venv found at git toplevel"
            );
            return Ok(Some(venv_path));
        }
    } else {
        tracing::debug!("No git toplevel found, skipping toplevel check");
    }

    // 3. Search for .venv from cwd
    let venv_path = cwd.join(VENV_DIR);
    let pyvenv_cfg = venv_path.join(PYVENV_CFG);

    tracing::debug!(
        cwd = %cwd.display(),
        checking_path = %venv_path.display(),
        pyvenv_cfg = %pyvenv_cfg.display(),
        exists = pyvenv_cfg.exists(),
        "Checking cwd for .venv"
    );

    if pyvenv_cfg.exists() {
        tracing::info!(
            venv = %venv_path.display(),
            "Fallback .venv found at cwd"
        );
        return Ok(Some(venv_path));
    }

    tracing::warn!(
        cwd = %cwd.display(),
        git_toplevel = ?git_toplevel.as_ref().map(|p| p.display().to_string()),
        "No fallback .venv found"
    );
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use tokio::fs;

    #[tokio::test]
    async fn test_find_venv() {
        let temp = tempdir().unwrap();
        let venv = temp.path().join(".venv");
        fs::create_dir(&venv).await.unwrap();
        fs::write(venv.join("pyvenv.cfg"), "home = /usr/bin")
            .await
            .unwrap();

        let subdir = temp.path().join("subdir");
        fs::create_dir(&subdir).await.unwrap();
        let file = subdir.join("test.py");
        fs::write(&file, "# test").await.unwrap();

        let result = find_venv(&file, None).await.unwrap();
        assert_eq!(result, Some(venv));
    }

    #[tokio::test]
    async fn test_find_venv_not_found() {
        let temp = tempdir().unwrap();
        let file = temp.path().join("test.py");
        fs::write(&file, "# test").await.unwrap();

        let result = find_venv(&file, None).await.unwrap();
        assert_eq!(result, None);
    }
}
