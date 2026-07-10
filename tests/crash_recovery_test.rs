mod support;

use support::{PackageConfig, ProxyUnderTest, WorkspaceConfig};

/// E2E: Backend crash recovery — proxy detects crash, clears diagnostics,
/// and auto-spawns a new backend on the next request.
///
/// 1st lifetime: hover succeeds, then didOpen triggers crash (exit 1)
/// 2nd lifetime: scenario rewritten on disk, hover succeeds with new backend
#[tokio::test]
// `file_a`/`file_b` (and their `_uri` variants) are deliberately parallel names
// for the two test fixtures this scenario exercises.
#[allow(clippy::similar_names)]
async fn backend_crash_recovery() {
    // First lifetime scenario: hover works, then crash on didOpen.
    let scenario_life1 = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [{ "type": "respond", "body": { "capabilities": { "hoverProvider": true } } }]
            },
            { "expect": { "method": "initialized" }, "actions": [] },
            // dispatch_initialized forwards a 2nd "initialized" to fallback backends
            { "expect": { "method": "initialized" }, "actions": [] },
            { "expect": { "method": "textDocument/didOpen" }, "actions": [] },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover before crash" } } }]
            },
            {
                "expect": { "method": "textDocument/didOpen" },
                "actions": [{ "type": "crash" }]
            }
        ]
    });

    let config = WorkspaceConfig {
        packages: vec![PackageConfig {
            name: "pkg".to_string(),
            scenario: scenario_life1,
            has_venv: true,
        }],
    };

    let (temp_dir, root) = support::setup_test_workspace(&config);
    // Start proxy from pkg/ (has .venv → backend spawns immediately).
    let mut proxy = ProxyUnderTest::spawn(temp_dir, root.clone(), &root.join("pkg"));

    let root_uri = support::path_to_uri(&root.join("pkg"));
    let init_resp = proxy.initialize(&root_uri).await;
    assert!(
        init_resp.error.is_none(),
        "initialize should not return an error"
    );
    proxy.send_initialized().await;

    // didOpen a.py
    let file_a = root.join("pkg/a.py");
    std::fs::write(&file_a, "a = 1\n").unwrap();
    let file_a_uri = support::path_to_uri(&file_a);
    proxy.did_open(&file_a_uri, "a = 1\n").await;

    // Hover on a.py → first lifetime
    let hover1 = proxy
        .request(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": &file_a_uri },
                "position": { "line": 0, "character": 0 }
            }),
        )
        .await;
    assert!(
        hover1.error.is_none(),
        "hover before crash should succeed, got error: {:?}",
        hover1.error
    );
    assert_eq!(
        hover1.result.as_ref().unwrap()["contents"]["value"],
        "hover before crash"
    );

    // didOpen b.py → triggers crash (exit 1)
    let file_b = root.join("pkg/b.py");
    std::fs::write(&file_b, "b = 2\n").unwrap();
    let file_b_uri = support::path_to_uri(&file_b);
    proxy.did_open(&file_b_uri, "b = 2\n").await;

    // Wait for crash cleanup: expect 2 empty publishDiagnostics (a.py + b.py)
    let notifications = proxy.wait_for_crash_cleanup(2, 5000).await;
    let diag_count = notifications
        .iter()
        .filter(|m| m.method.as_deref() == Some("textDocument/publishDiagnostics"))
        .count();
    assert!(
        diag_count >= 2,
        "expected at least 2 publishDiagnostics, got {diag_count}"
    );

    // Rewrite scenario.json for the second lifetime.
    let scenario_life2 = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [{ "type": "respond", "body": { "capabilities": { "hoverProvider": true } } }]
            },
            { "expect": { "method": "initialized" }, "actions": [] },
            // restore_open_documents sends didOpen for a.py and b.py (order non-deterministic)
            { "expect": { "method": "textDocument/didOpen" }, "actions": [] },
            { "expect": { "method": "textDocument/didOpen" }, "actions": [] },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover after recovery" } } }]
            },
            {
                "expect": { "method": "shutdown" },
                "actions": [{ "type": "respond", "body": null }]
            }
        ]
    });
    let scenario_path = proxy.root().join("pkg/.venv/scenario.json");
    let scenario_json = serde_json::to_string_pretty(&scenario_life2).unwrap();
    std::fs::write(&scenario_path, &scenario_json).unwrap();

    // Hover on a.py → proxy auto-spawns new backend (2nd lifetime)
    let hover2 = proxy
        .request(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": &file_a_uri },
                "position": { "line": 0, "character": 0 }
            }),
        )
        .await;
    assert!(
        hover2.error.is_none(),
        "hover after recovery should succeed, got error: {:?}",
        hover2.error
    );
    assert_eq!(
        hover2.result.as_ref().unwrap()["contents"]["value"],
        "hover after recovery"
    );

    // Shutdown
    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}
