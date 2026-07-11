mod support;

use std::collections::HashMap;
use support::{PackageConfig, ProxyUnderTest, WorkspaceConfig};
use typemux_cc::message::{RpcId, RpcMessage};

/// E2E: while a backend is warming, an index-dependent request
/// (`textDocument/definition`) is queued rather than forwarded, and a
/// non-index-dependent request (`textDocument/hover`) passes through
/// immediately. Once the backend signals readiness via its recorded
/// indexing token's `$/progress` end, the queued request is drained and
/// answered. The `window/workDoneProgress/create` request is what records
/// "warmup" as that token in the first place (see #104) — without it,
/// warmup would never see a matching `end` and this would fall back to the
/// timeout instead of draining on the progress signal this test targets.
///
/// Determinism: the proxy processes client messages strictly in the order
/// they arrive on its stdin, so sending `definition` and then a second
/// `hover` (with no intervening await) guarantees the proxy queues
/// `definition` before it ever forwards the second `hover` to the backend
/// — the backend's own `$/progress` end (attached to that second hover's
/// reply) can therefore only be sent after `definition` is already queued.
/// No sleeps are needed; see `tests/venv_staleness_test.rs` for the
/// alternative (env-injected interval + sleep) pattern used when a real
/// wall-clock race can't be avoided.
#[tokio::test]
async fn queued_definition_drains_after_progress_end_while_hover_passes_through() {
    let scenario = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [{ "type": "respond", "body": { "capabilities": { "hoverProvider": true, "definitionProvider": true } } }]
            },
            { "expect": { "method": "initialized" }, "actions": [] },
            { "expect": { "method": "textDocument/didOpen" }, "actions": [] },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover before ready" } } }]
            },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [
                    { "type": "request", "id": 100, "method": "window/workDoneProgress/create", "params": { "token": "warmup" } },
                    { "type": "notify", "method": "$/progress", "params": { "token": "warmup", "value": { "kind": "end" } } },
                    { "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover triggering ready" } } }
                ]
            },
            {
                "expect": { "method": "textDocument/definition" },
                "actions": [{ "type": "respond", "body": { "uri": "file:///pkg/a.py", "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } } } }]
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
    // Start from workspace root (pool-miss spawn, single "initialized").
    // Warmup timeout is generous: this test drives warmup completion via
    // the $/progress signal, not the timeout (see the fail-open test below
    // for that path), so the margin is just insurance against slow CI.
    let mut proxy = ProxyUnderTest::spawn_with_env(
        temp_dir,
        root.clone(),
        &root,
        &[("TYPEMUX_CC_WARMUP_TIMEOUT", "10")],
    );

    let root_uri = support::path_to_uri(&root);
    let init_resp = proxy.initialize(&root_uri).await;
    assert!(
        init_resp.error.is_none(),
        "initialize should not return an error"
    );
    proxy.send_initialized().await;

    let file_a = root.join("pkg/a.py");
    std::fs::write(&file_a, "a = 1\n").unwrap();
    let file_a_uri = support::path_to_uri(&file_a);
    proxy.did_open(&file_a_uri, "a = 1\n").await;

    let doc_params = || {
        serde_json::json!({
            "textDocument": { "uri": &file_a_uri },
            "position": { "line": 0, "character": 0 }
        })
    };

    // First hover: spawns the backend (still warming) and passes straight
    // through, since hover is not index-dependent.
    let hover1 = proxy.request("textDocument/hover", doc_params()).await;
    assert!(
        hover1.error.is_none(),
        "first hover should not return an error, got: {:?}",
        hover1.error
    );
    assert_eq!(
        hover1.result.as_ref().unwrap()["contents"]["value"],
        "hover before ready"
    );

    // definition is index-dependent and the backend is still warming: it
    // must be queued, not forwarded. Sent before the second hover so the
    // proxy queues it first (see the doc comment above on ordering).
    let def_id = proxy
        .send_request("textDocument/definition", doc_params())
        .await;

    // Second hover: still passes through immediately (not index-dependent).
    // Its scripted reply carries the $/progress end that transitions the
    // backend to Ready and drains the queued definition request.
    let hover2_id = proxy.send_request("textDocument/hover", doc_params()).await;

    // Filtered to `is_response()`: the backend's `window/workDoneProgress/create`
    // request is also forwarded to the client in this window (with its own
    // proxy-assigned id — see #104), and would otherwise be miscounted as
    // one of the two responses this loop is waiting for.
    let mut responses: HashMap<i64, RpcMessage> = HashMap::new();
    while responses.len() < 2 {
        let msg = proxy.read_next().await;
        if msg.is_response() {
            if let Some(RpcId::Number(id)) = &msg.id {
                responses.insert(*id, msg);
            }
        }
    }

    let hover2 = &responses[&hover2_id];
    assert!(
        hover2.error.is_none(),
        "second hover should not return an error, got: {:?}",
        hover2.error
    );
    assert_eq!(
        hover2.result.as_ref().unwrap()["contents"]["value"],
        "hover triggering ready"
    );

    let def_resp = &responses[&def_id];
    assert!(
        def_resp.error.is_none(),
        "queued definition should be drained and answered, got: {:?}",
        def_resp.error
    );
    assert_eq!(def_resp.result.as_ref().unwrap()["uri"], "file:///pkg/a.py");

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}

/// E2E: an index-dependent request queued during warmup is drained
/// (forwarded, not left hanging or errored) once the warmup timeout
/// elapses, even though the backend never sends a `$/progress` end event
/// (fail-open).
#[tokio::test]
async fn warmup_fail_open_drains_queue_after_timeout() {
    let scenario = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [{ "type": "respond", "body": { "capabilities": { "definitionProvider": true } } }]
            },
            { "expect": { "method": "initialized" }, "actions": [] },
            { "expect": { "method": "textDocument/didOpen" }, "actions": [] },
            {
                "expect": { "method": "textDocument/definition" },
                "actions": [{ "type": "respond", "body": { "uri": "file:///pkg/a.py", "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } } } }]
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
    // Short warmup timeout: the mock never sends $/progress end, so the
    // queued request can only ever be drained via the timeout's fail-open
    // path. The harness's 5s read timeout leaves ample margin over 1s.
    let mut proxy = ProxyUnderTest::spawn_with_env(
        temp_dir,
        root.clone(),
        &root,
        &[("TYPEMUX_CC_WARMUP_TIMEOUT", "1")],
    );

    let root_uri = support::path_to_uri(&root);
    let init_resp = proxy.initialize(&root_uri).await;
    assert!(
        init_resp.error.is_none(),
        "initialize should not return an error"
    );
    proxy.send_initialized().await;

    let file_a = root.join("pkg/a.py");
    std::fs::write(&file_a, "a = 1\n").unwrap();
    let file_a_uri = support::path_to_uri(&file_a);
    proxy.did_open(&file_a_uri, "a = 1\n").await;

    // Queued while warming; only answered once the 1s timeout fail-opens
    // the backend to Ready and drains the queue.
    let def_resp = proxy
        .request(
            "textDocument/definition",
            serde_json::json!({
                "textDocument": { "uri": &file_a_uri },
                "position": { "line": 0, "character": 0 }
            }),
        )
        .await;
    assert!(
        def_resp.error.is_none(),
        "definition queued during warmup should be drained after fail-open, got: {:?}",
        def_resp.error
    );
    assert_eq!(def_resp.result.as_ref().unwrap()["uri"], "file:///pkg/a.py");

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}
