mod support;

use support::{PackageConfig, ProxyUnderTest, WorkspaceConfig};

/// E2E: Two venv-backed packages route hover requests to the correct backend.
///
/// - proj-a hover → "hover from backend-a"
/// - proj-b hover → "hover from backend-b"
/// - proj-a hover again → still "hover from backend-a" (no cross-contamination)
#[tokio::test]
// `file_a`/`file_b` (and their `_uri` variants) are deliberately parallel names
// for the two test fixtures this scenario exercises.
#[allow(clippy::similar_names)]
async fn multi_venv_switching() {
    let scenario_a = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [{ "type": "respond", "body": { "capabilities": { "hoverProvider": true } } }]
            },
            { "expect": { "method": "initialized" }, "actions": [] },
            { "expect": { "method": "textDocument/didOpen" }, "actions": [] },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover from backend-a" } } }]
            },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover from backend-a" } } }]
            },
            {
                "expect": { "method": "shutdown" },
                "actions": [{ "type": "respond", "body": null }]
            }
        ]
    });

    let scenario_b = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [{ "type": "respond", "body": { "capabilities": { "hoverProvider": true } } }]
            },
            { "expect": { "method": "initialized" }, "actions": [] },
            { "expect": { "method": "textDocument/didOpen" }, "actions": [] },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover from backend-b" } } }]
            },
            {
                "expect": { "method": "shutdown" },
                "actions": [{ "type": "respond", "body": null }]
            }
        ]
    });

    let config = WorkspaceConfig {
        packages: vec![
            PackageConfig {
                name: "proj-a".to_string(),
                scenario: scenario_a,
                has_venv: true,
            },
            PackageConfig {
                name: "proj-b".to_string(),
                scenario: scenario_b,
                has_venv: true,
            },
        ],
    };

    let (temp_dir, root) = support::setup_test_workspace(&config);
    // Start proxy from workspace root (no fallback venv at root level).
    let mut proxy = ProxyUnderTest::spawn(temp_dir, root.clone(), &root);

    let root_uri = support::path_to_uri(&root);
    let init_resp = proxy.initialize(&root_uri).await;
    assert!(
        init_resp.error.is_none(),
        "initialize should not return an error"
    );
    proxy.send_initialized().await;

    // didOpen for proj-a → spawns backend-a
    let file_a = root.join("proj-a/main.py");
    std::fs::write(&file_a, "a = 1\n").unwrap();
    let file_a_uri = support::path_to_uri(&file_a);
    proxy.did_open(&file_a_uri, "a = 1\n").await;

    // Hover on proj-a
    let hover_a = proxy
        .request(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": &file_a_uri },
                "position": { "line": 0, "character": 0 }
            }),
        )
        .await;
    assert!(hover_a.error.is_none(), "hover on proj-a should succeed");
    assert_eq!(
        hover_a.result.as_ref().unwrap()["contents"]["value"],
        "hover from backend-a"
    );

    // didOpen for proj-b → spawns backend-b
    let file_b = root.join("proj-b/main.py");
    std::fs::write(&file_b, "b = 2\n").unwrap();
    let file_b_uri = support::path_to_uri(&file_b);
    proxy.did_open(&file_b_uri, "b = 2\n").await;

    // Hover on proj-b
    let hover_b = proxy
        .request(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": &file_b_uri },
                "position": { "line": 0, "character": 0 }
            }),
        )
        .await;
    assert!(hover_b.error.is_none(), "hover on proj-b should succeed");
    assert_eq!(
        hover_b.result.as_ref().unwrap()["contents"]["value"],
        "hover from backend-b"
    );

    // Hover on proj-a again → still routes to backend-a (no cross-contamination)
    let hover_a2 = proxy
        .request(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": &file_a_uri },
                "position": { "line": 0, "character": 0 }
            }),
        )
        .await;
    assert!(
        hover_a2.error.is_none(),
        "second hover on proj-a should succeed"
    );
    assert_eq!(
        hover_a2.result.as_ref().unwrap()["contents"]["value"],
        "hover from backend-a"
    );

    // Shutdown
    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}
