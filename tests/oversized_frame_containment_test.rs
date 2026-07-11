mod support;

use support::{PackageConfig, ProxyUnderTest, WorkspaceConfig};

/// E2E (#106, #92 containment): a backend that becomes `Ready` and answers
/// one request normally, then emits a raw frame declaring a `Content-Length`
/// far beyond the framing limit, is contained — the reader task's framing
/// error routes through the existing crash-cleanup path
/// (`handle_backend_crash`, unchanged by this fix), evicting just that
/// backend, while a second, healthy venv keeps serving normally.
///
/// The garbage bytes are appended as a second action on the same step that
/// answers the hover request (not tied to any further client message): the
/// reader task, already looping in the background per
/// `backend_pool::spawn_reader_task`, picks it up on its own next read.
#[tokio::test]
async fn oversized_frame_from_backend_is_contained_e2e() {
    let scenario_good = serde_json::json!({
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
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover good sync" } } }]
            },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover good again" } } }]
            },
            {
                "expect": { "method": "shutdown" },
                "actions": [{ "type": "respond", "body": null }]
            }
        ]
    });

    // Answers one hover normally (proving it reached Ready and the reader
    // task is actively draining its stdout), then immediately follows up
    // with a malformed frame: a Content-Length far past the framing limit,
    // no body. The proxy must reject it before attempting to allocate or
    // read a body, and evict this backend without disturbing "good".
    let scenario_garbage = serde_json::json!({
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
                "actions": [
                    { "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover garbage sync" } } },
                    { "type": "raw_bytes", "data": "Content-Length: 999999999999\r\n\r\n" }
                ]
            }
        ]
    });

    let config = WorkspaceConfig {
        packages: vec![
            PackageConfig {
                name: "good".to_string(),
                scenario: scenario_good,
                has_venv: true,
            },
            PackageConfig {
                name: "garbage".to_string(),
                scenario: scenario_garbage,
                has_venv: true,
            },
        ],
    };

    let (temp_dir, root) = support::setup_test_workspace(&config);
    let mut proxy = ProxyUnderTest::spawn(temp_dir, root.clone(), &root);

    let root_uri = support::path_to_uri(&root);
    let init_resp = proxy.initialize(&root_uri).await;
    assert!(
        init_resp.error.is_none(),
        "initialize should not return an error"
    );
    proxy.send_initialized().await;

    let file_good = root.join("good/main.py");
    std::fs::write(&file_good, "a = 1\n").unwrap();
    let file_good_uri = support::path_to_uri(&file_good);
    proxy.did_open(&file_good_uri, "a = 1\n").await;

    let hover_good_params = || {
        serde_json::json!({
            "textDocument": { "uri": &file_good_uri },
            "position": { "line": 0, "character": 0 }
        })
    };
    let sync_good = proxy
        .request("textDocument/hover", hover_good_params())
        .await;
    assert!(
        sync_good.error.is_none(),
        "sync hover(good) failed: {:?}",
        sync_good.error
    );

    let file_garbage = root.join("garbage/main.py");
    std::fs::write(&file_garbage, "g = 1\n").unwrap();
    let file_garbage_uri = support::path_to_uri(&file_garbage);
    proxy.did_open(&file_garbage_uri, "g = 1\n").await;

    let hover_garbage = proxy
        .request(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": &file_garbage_uri },
                "position": { "line": 0, "character": 0 }
            }),
        )
        .await;
    assert!(
        hover_garbage.error.is_none(),
        "hover(garbage) sync failed: {:?}",
        hover_garbage.error
    );
    assert_eq!(
        hover_garbage.result.as_ref().unwrap()["contents"]["value"],
        "hover garbage sync"
    );

    // Crash cleanup for the garbage venv: 1 open doc -> 1 empty publishDiagnostics.
    let notifications = proxy.wait_for_crash_cleanup(1, 5000).await;
    let diag_count = notifications
        .iter()
        .filter(|m| {
            m.method.as_deref() == Some("textDocument/publishDiagnostics")
                && m.params.as_ref().and_then(|p| p["uri"].as_str())
                    == Some(file_garbage_uri.as_str())
        })
        .count();
    assert!(
        diag_count >= 1,
        "expected crash cleanup diagnostics for the garbage venv, got {diag_count}"
    );

    // The good venv must be unaffected by the garbage venv's containment.
    let hover_good2 = proxy
        .request("textDocument/hover", hover_good_params())
        .await;
    assert!(
        hover_good2.error.is_none(),
        "hover(good) after garbage-venv crash failed: {:?}",
        hover_good2.error
    );
    assert_eq!(
        hover_good2.result.as_ref().unwrap()["contents"]["value"],
        "hover good again"
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}
