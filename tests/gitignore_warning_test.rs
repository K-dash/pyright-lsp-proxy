mod support;

use support::{PackageConfig, ProxyUnderTest, WorkspaceConfig};

/// E2E: warn the client when a venv's project root is gitignored from the
/// proxy's cwd. Claude Code's LSP tool filters goToDefinition/findReferences
/// results through `git check-ignore` run in the session cwd, silently
/// dropping gitignored locations (anthropics/claude-code#76371).
///
/// Workspace: `pkg/` has `.venv`, and the workspace root's `.gitignore`
/// excludes `pkg/`. The proxy is started with cwd = workspace root (not
/// `pkg/`), so the fallback venv is not found and the backend for `pkg`
/// spawns lazily on the first `didOpen`.
#[tokio::test]
async fn warns_when_project_root_is_gitignored() {
    let scenario = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [{ "type": "respond", "body": { "capabilities": { "hoverProvider": true } } }]
            },
            { "expect": { "method": "initialized" }, "actions": [] },
            // restore_documents_to_backend replays didOpen for a.py (opened before the backend spawned).
            { "expect": { "method": "textDocument/didOpen" }, "actions": [] },
            // Second didOpen for b.py is forwarded directly (backend already in pool).
            { "expect": { "method": "textDocument/didOpen" }, "actions": [] },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover on b.py" } } }]
            },
            {
                "expect": { "method": "shutdown" },
                "actions": [{ "type": "respond", "body": null }]
            }
        ]
    });

    let config = WorkspaceConfig {
        packages: vec![PackageConfig {
            name: "pkg".to_string(),
            scenario,
            has_venv: true,
        }],
    };

    let (temp_dir, root) = support::setup_test_workspace(&config);

    // Turn the fixture's minimal `.git` into a real repository and gitignore `pkg/`.
    std::process::Command::new("git")
        .arg("init")
        .current_dir(&root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .expect("failed to run git init");
    std::fs::write(root.join(".gitignore"), "pkg/\n").unwrap();

    // Start the proxy from the workspace root (no `.venv` there) so no
    // fallback backend is pre-spawned; the `pkg` backend spawns on didOpen.
    let mut proxy = ProxyUnderTest::spawn(temp_dir, root.clone(), &root);

    let root_uri = support::path_to_uri(&root);
    let init_resp = proxy.initialize(&root_uri).await;
    assert!(
        init_resp.error.is_none(),
        "initialize should not return an error"
    );
    proxy.send_initialized().await;

    // didOpen a.py under pkg/ → proxy lazily spawns the backend for pkg/.venv.
    let file_a = root.join("pkg/a.py");
    std::fs::write(&file_a, "a = 1\n").unwrap();
    let file_a_uri = support::path_to_uri(&file_a);
    proxy.did_open(&file_a_uri, "a = 1\n").await;

    // The proxy must warn that pkg/'s project root is gitignored from its cwd.
    let warning = proxy
        .wait_for_notification("window/showMessage", 5000)
        .await;
    let params = warning.params.expect("window/showMessage must have params");
    assert_eq!(params["type"], 2, "expected Warning severity");
    let message = params["message"]
        .as_str()
        .expect("message field must be a string");
    assert!(
        message.contains("gitignored"),
        "expected warning message to mention 'gitignored', got: {message}"
    );
    assert!(
        message.contains(&root.join("pkg").display().to_string()),
        "expected warning message to mention the project root, got: {message}"
    );

    // didOpen b.py under the same venv must not trigger a duplicate warning.
    let file_b = root.join("pkg/b.py");
    std::fs::write(&file_b, "b = 2\n").unwrap();
    let file_b_uri = support::path_to_uri(&file_b);
    proxy.did_open(&file_b_uri, "b = 2\n").await;

    let (hover_resp, notifications) = proxy
        .request_collecting(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": &file_b_uri },
                "position": { "line": 0, "character": 0 }
            }),
        )
        .await;
    assert!(
        hover_resp.error.is_none(),
        "hover on b.py should succeed, got error: {:?}",
        hover_resp.error
    );
    assert!(
        !notifications
            .iter()
            .any(|n| n.method.as_deref() == Some("window/showMessage")),
        "gitignored project root warning must not be sent twice for the same venv"
    );

    // Shutdown
    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}
