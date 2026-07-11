mod support;

use support::{PackageConfig, ProxyUnderTest, WorkspaceConfig};

/// E2E: repeated backend-creation failures for the same venv must not spam
/// the client with `window/showMessage` notifications (issue #26).
///
/// `pkg/.venv` is broken from the start (fake `pyright-langserver` exits
/// immediately), so every `didOpen` under it hits the pool-miss path in
/// `handle_did_open` and fails to spawn a backend. Opening 5 files in quick
/// succession must produce exactly one client-visible error notification;
/// the rest are suppressed by the per-venv TTL dedup in
/// `notify_backend_error`, though each failure is still logged server-side.
#[tokio::test]
async fn repeated_broken_venv_failures_notify_once() {
    let config = WorkspaceConfig {
        packages: vec![PackageConfig {
            name: "pkg".to_string(),
            scenario: serde_json::json!({ "on_startup": [], "steps": [] }),
            has_venv: false,
        }],
    };

    let (temp_dir, root) = support::setup_test_workspace(&config);
    let pkg_dir = root.join("pkg");
    support::write_broken_venv_fixture(&pkg_dir);

    // Start the proxy from the workspace root (no `.venv` there), so no
    // fallback backend is pre-spawned; the `pkg` backend spawns lazily on
    // didOpen and fails every time.
    let mut proxy = ProxyUnderTest::spawn(temp_dir, root.clone(), &root);

    let root_uri = support::path_to_uri(&root);
    let init_resp = proxy.initialize(&root_uri).await;
    assert!(
        init_resp.error.is_none(),
        "initialize should not return an error"
    );
    proxy.send_initialized().await;

    // Open 5 files under the same broken venv in quick succession.
    for name in ["a", "b", "c", "d", "e"] {
        let file = pkg_dir.join(format!("{name}.py"));
        std::fs::write(&file, format!("{name} = 1\n")).unwrap();
        let file_uri = support::path_to_uri(&file);
        proxy.did_open(&file_uri, &format!("{name} = 1\n")).await;
    }

    // Exactly one error notification should surface for all 5 failures.
    let notification = proxy
        .wait_for_notification("window/showMessage", 5000)
        .await;
    let params = notification
        .params
        .expect("window/showMessage must have params");
    assert_eq!(params["type"], 1, "expected Error severity");
    let message = params["message"]
        .as_str()
        .expect("message field must be a string");
    assert!(
        message.contains("Failed to start LSP backend"),
        "expected backend-failure message, got: {message}"
    );

    // A further request against the same broken venv must not trigger a
    // second notification: it fails via the request/response path (a JSON-RPC
    // error, not `notify_backend_error`), and the dedup window is still open.
    let file_a_uri = support::path_to_uri(&pkg_dir.join("a.py"));
    let (hover_resp, notifications) = proxy
        .request_collecting(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": &file_a_uri },
                "position": { "line": 0, "character": 0 }
            }),
        )
        .await;
    let error = hover_resp
        .error
        .expect("hover against the broken venv must fail explicitly");
    assert!(
        error.message.contains("backend error"),
        "expected a backend error response, got: {}",
        error.message
    );
    assert!(
        !notifications
            .iter()
            .any(|n| n.method.as_deref() == Some("window/showMessage")),
        "no additional showMessage notification should be sent within the dedup TTL"
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}
