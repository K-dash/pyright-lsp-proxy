use crate::error::VenvError;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tokio::process::Command;

const VENV_DIR: &str = ".venv";
const PYVENV_CFG: &str = "pyvenv.cfg";

/// Identity token for a `.venv`, derived from `.venv/pyvenv.cfg` metadata.
///
/// `uv sync` recreating `.venv` allocates a new inode (`dev`/`ino` change);
/// in-place edits change `mtime`/`size`. `size` hardens against the rare case
/// of inode reuse combined with a coarse mtime collision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VenvToken {
    dev: u64,
    ino: u64,
    mtime: SystemTime,
    size: u64,
}

/// Stat `.venv/pyvenv.cfg` and capture its identity token.
/// Returns `None` if the file cannot be stat'd (missing, permission error, etc.).
pub async fn venv_token(venv: &Path) -> Option<VenvToken> {
    let meta = tokio::fs::metadata(venv.join(PYVENV_CFG)).await.ok()?;
    let mtime = meta.modified().ok()?;
    Some(VenvToken {
        dev: meta.dev(),
        ino: meta.ino(),
        mtime,
        size: meta.len(),
    })
}

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
        // `git rev-parse --show-toplevel` already resolves symlinks, but
        // canonicalize defensively so this is guaranteed to match the form
        // `find_venv` canonicalizes the file path to.
        let path = path.canonicalize().unwrap_or(path);
        tracing::info!(toplevel = %path.display(), "Git toplevel found");
        Ok(Some(path))
    } else {
        tracing::warn!("Not in a git repository");
        Ok(None)
    }
}

/// Execute `git check-ignore -q <path>` and check whether it is gitignored from cwd
///
/// Clears `GIT_DIR`/`GIT_WORK_TREE`/`GIT_INDEX_FILE` so the check always resolves
/// against `cwd` as requested, instead of silently deferring to an ambient
/// repository override (e.g. when the proxy is launched from inside a
/// git hook environment).
pub async fn is_path_git_ignored(path: &Path, cwd: &Path) -> bool {
    let output = match Command::new("git")
        .arg("check-ignore")
        .arg("-q")
        .arg(path)
        .current_dir(cwd)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .await
    {
        Ok(output) => output,
        Err(e) => {
            tracing::warn!(error = ?e, "git command failed (git not installed or not executable), treating path as not ignored");
            return false;
        }
    };

    output.status.success()
}

/// Search for .venv by traversing parent directories from file path
///
/// # Arguments
/// * `file_path` - Starting file path
/// * `git_toplevel` - Search boundary (if None, search up to root)
pub fn find_venv(file_path: &Path, git_toplevel: Option<&Path>) -> Option<PathBuf> {
    tracing::debug!(
        file = %file_path.display(),
        toplevel = ?git_toplevel.map(|p| p.display().to_string()),
        "Starting .venv search"
    );

    // Canonicalize the starting directory so the upward walk and the
    // `git_toplevel` boundary (already canonicalized in `get_git_toplevel`)
    // agree on symlink resolution. Falls back to the raw path when the file
    // (or its parent) does not exist yet, e.g. an unsaved buffer.
    let start = file_path
        .parent()
        .map(|dir| dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf()));
    let mut current = start.as_deref();
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
            return Some(venv_path);
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
    None
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
        // Canonicalize to resolve symlinks (e.g., /var -> /private/var on
        // macOS): find_venv now returns a canonical path, so the expected
        // value must be canonical too.
        let temp = tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let venv = root.join(".venv");
        fs::create_dir(&venv).await.unwrap();
        fs::write(venv.join("pyvenv.cfg"), "home = /usr/bin")
            .await
            .unwrap();

        let subdir = root.join("subdir");
        fs::create_dir(&subdir).await.unwrap();
        let file = subdir.join("test.py");
        fs::write(&file, "# test").await.unwrap();

        let result = find_venv(&file, None);
        assert_eq!(result, Some(venv));
    }

    #[tokio::test]
    async fn test_find_venv_resolves_symlinked_project_dir() {
        // The file path reaches the project through a symlink (mimicking a
        // client URI with a logical path like macOS's /tmp -> /private/tmp),
        // while `find_venv` walks physical directories. The returned venv
        // path must still be canonical so it matches pool keys derived from
        // the canonicalized git toplevel.
        let temp = tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();

        let real_project = root.join("real_project");
        fs::create_dir(&real_project).await.unwrap();
        let venv = real_project.join(".venv");
        fs::create_dir(&venv).await.unwrap();
        fs::write(venv.join("pyvenv.cfg"), "home = /usr/bin")
            .await
            .unwrap();

        let symlink_project = root.join("symlinked_project");
        std::os::unix::fs::symlink(&real_project, &symlink_project).unwrap();

        let file = symlink_project.join("test.py");
        fs::write(real_project.join("test.py"), "# test")
            .await
            .unwrap();

        let result = find_venv(&file, None);
        assert_eq!(result, Some(venv));
    }

    #[tokio::test]
    async fn test_find_venv_respects_boundary_under_symlink() {
        // Boundary check must still hold once both sides are canonicalized:
        // a venv that lives above the (canonicalized) git toplevel must not
        // be adopted, even when the file is reached through a symlink.
        let temp = tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();

        // Outer .venv, above the toplevel — must never be picked up.
        let outer_venv = root.join(".venv");
        fs::create_dir(&outer_venv).await.unwrap();
        fs::write(outer_venv.join("pyvenv.cfg"), "home = /usr/bin")
            .await
            .unwrap();

        let real_repo = root.join("real_repo");
        fs::create_dir(&real_repo).await.unwrap();

        let symlink_repo = root.join("symlinked_repo");
        std::os::unix::fs::symlink(&real_repo, &symlink_repo).unwrap();

        let file = symlink_repo.join("test.py");
        fs::write(real_repo.join("test.py"), "# test")
            .await
            .unwrap();

        // Toplevel is the (canonical) real repo dir — no .venv inside it.
        let result = find_venv(&file, Some(&real_repo));
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_find_venv_not_found() {
        let temp = tempdir().unwrap();
        let file = temp.path().join("test.py");
        fs::write(&file, "# test").await.unwrap();

        let result = find_venv(&file, None);
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_is_path_git_ignored_matches_gitignore() {
        // Canonicalize to resolve symlinks (e.g., /var → /private/var on macOS).
        let temp = tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::process::Command::new("git")
            .arg("init")
            .current_dir(&root)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .unwrap();
        fs::write(root.join(".gitignore"), "ignored-dir/\n")
            .await
            .unwrap();
        let ignored_dir = root.join("ignored-dir");
        fs::create_dir(&ignored_dir).await.unwrap();

        assert!(is_path_git_ignored(&ignored_dir, &root).await);
    }

    #[tokio::test]
    async fn test_is_path_git_ignored_not_matched() {
        let temp = tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::process::Command::new("git")
            .arg("init")
            .current_dir(&root)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .unwrap();
        fs::write(root.join(".gitignore"), "ignored-dir/\n")
            .await
            .unwrap();
        let other_dir = root.join("other-dir");
        fs::create_dir(&other_dir).await.unwrap();

        assert!(!is_path_git_ignored(&other_dir, &root).await);
    }

    #[tokio::test]
    async fn test_is_path_git_ignored_no_git_repo() {
        let temp = tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let some_dir = root.join("some-dir");
        fs::create_dir(&some_dir).await.unwrap();

        assert!(!is_path_git_ignored(&some_dir, &root).await);
    }

    #[tokio::test]
    async fn test_venv_token_stable_across_restat() {
        let temp = tempdir().unwrap();
        let venv = temp.path().join(".venv");
        fs::create_dir(&venv).await.unwrap();
        fs::write(venv.join("pyvenv.cfg"), "home = /usr/bin")
            .await
            .unwrap();

        let token1 = venv_token(&venv).await;
        let token2 = venv_token(&venv).await;
        assert!(token1.is_some());
        assert_eq!(token1, token2);
    }

    #[tokio::test]
    async fn test_venv_token_changes_on_recreate() {
        let temp = tempdir().unwrap();
        let venv = temp.path().join(".venv");
        fs::create_dir(&venv).await.unwrap();
        fs::write(venv.join("pyvenv.cfg"), "home = /usr/bin")
            .await
            .unwrap();
        let token1 = venv_token(&venv).await;

        // Recreate with different content (different size and inode).
        fs::remove_file(venv.join("pyvenv.cfg")).await.unwrap();
        fs::write(venv.join("pyvenv.cfg"), "home = /usr/bin\nversion = 2")
            .await
            .unwrap();
        let token2 = venv_token(&venv).await;

        assert!(token1.is_some());
        assert!(token2.is_some());
        assert_ne!(token1, token2);
    }

    #[tokio::test]
    async fn test_venv_token_missing_is_none() {
        let temp = tempdir().unwrap();
        let venv = temp.path().join(".venv");

        assert!(venv_token(&venv).await.is_none());
    }
}
