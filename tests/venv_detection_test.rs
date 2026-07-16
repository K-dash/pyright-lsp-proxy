mod support;

use support::{PackageConfig, ProxyUnderTest, WorkspaceConfig};

/// Priority 2: Venv detection — two packages, only one with `.venv`.
///
/// - `pkg-a` has `.venv` → backend auto-spawns on didOpen, hover works
/// - `pkg-b` has no `.venv` → hover returns error (strict mode, no fallback)
#[tokio::test]
// `file_a`/`file_b` (and their `_uri` variants) are deliberately parallel names
// for the two test fixtures this scenario exercises.
#[allow(clippy::similar_names)]
async fn venv_detection_routing() {
    let scenario_a = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [{
                    "type": "respond",
                    "body": {
                        "capabilities": {
                            "hoverProvider": true
                        }
                    }
                }]
            },
            {
                "expect": { "method": "initialized" },
                "actions": []
            },
            {
                "expect": { "method": "textDocument/didOpen" },
                "actions": []
            },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{
                    "type": "respond",
                    "body": {
                        "contents": { "kind": "plaintext", "value": "mock hover result" }
                    }
                }]
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
                name: "pkg-a".to_string(),
                scenario: scenario_a,
                has_venv: true,
            },
            PackageConfig {
                name: "pkg-b".to_string(),
                scenario: serde_json::json!({"steps": []}),
                has_venv: false,
            },
        ],
    };

    let (temp_dir, root) = support::setup_test_workspace(&config);
    // Start the proxy from the workspace root (above both packages).
    // No fallback venv at root level → proxy starts with empty pool.
    let mut proxy = ProxyUnderTest::spawn(temp_dir, root.clone(), &root);

    let root_uri = support::path_to_uri(&root);
    let init_resp = proxy.initialize(&root_uri).await;
    // initialize always answers with empty capabilities (#140)
    assert!(
        init_resp.error.is_none(),
        "initialize should not return an error"
    );

    proxy.send_initialized().await;

    // didOpen for pkg-a (has .venv) → backend auto-spawns
    let file_a = root.join("pkg-a/main.py");
    std::fs::write(&file_a, "x = 1\n").unwrap();
    let file_a_uri = support::path_to_uri(&file_a);
    proxy.did_open(&file_a_uri, "x = 1\n").await;

    // No sleep needed: the proxy's ensure_backend_in_pool synchronously
    // spawns and initializes the backend before returning the hover response.

    // Hover on pkg-a → should get response from mock backend
    let hover_resp = proxy
        .request(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": file_a_uri },
                "position": { "line": 0, "character": 0 }
            }),
        )
        .await;
    assert!(
        hover_resp.error.is_none(),
        "hover on pkg-a should succeed, got error: {:?}",
        hover_resp.error
    );
    let contents = &hover_resp.result.as_ref().unwrap()["contents"]["value"];
    assert_eq!(contents, "mock hover result");

    // didOpen for pkg-b (no .venv)
    let file_b = root.join("pkg-b/main.py");
    std::fs::write(&file_b, "y = 2\n").unwrap();
    let file_b_uri = support::path_to_uri(&file_b);
    proxy.did_open(&file_b_uri, "y = 2\n").await;

    // Hover on pkg-b → should get error (no venv, strict mode)
    let hover_resp_b = proxy
        .request(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": file_b_uri },
                "position": { "line": 0, "character": 0 }
            }),
        )
        .await;
    assert!(
        hover_resp_b.error.is_some(),
        "hover on pkg-b should fail (no .venv)"
    );

    // Shutdown
    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}
