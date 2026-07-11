mod support;

use support::{PackageConfig, ProxyUnderTest, WorkspaceConfig};

/// E2E: `$/cancelRequest` for a pending, already-forwarded request is
/// relayed to the owning backend, and a subsequent request on the same
/// backend still gets a normal response afterwards (proving
/// `pending_requests` bookkeeping wasn't corrupted by the cancellation).
///
/// The mock backend DSL can only respond to the request that satisfied the
/// *current* step's `expect` (`Action::Respond` always mirrors that
/// message's id), so it has no way to fabricate a JSON-RPC error response
/// tied to the *earlier* hover's id from within a later step. This test
/// therefore proves the contract observable through the DSL: forwarding
/// (the backend must see `$/cancelRequest` as the very next message after
/// the cancelled hover, or the scripted step order breaks and the mock
/// exits) and non-corruption (a fresh request on the same backend still
/// succeeds normally afterwards). See the report for the full reasoning.
#[tokio::test]
async fn cancel_request_forwarded_and_backend_still_usable() {
    let scenario = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [{ "type": "respond", "body": { "capabilities": { "hoverProvider": true } } }]
            },
            { "expect": { "method": "initialized" }, "actions": [] },
            { "expect": { "method": "textDocument/didOpen" }, "actions": [] },
            // First hover is deliberately left unanswered: it's the one
            // that gets cancelled.
            { "expect": { "method": "textDocument/hover" }, "actions": [] },
            // If $/cancelRequest were not forwarded, the next message the
            // backend actually sees is the second hover below instead of
            // this notification, which mismatches this step and crashes
            // the mock — a loud, detectable failure.
            { "expect": { "method": "$/cancelRequest" }, "actions": [] },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover after cancel" } } }]
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
    let mut proxy = ProxyUnderTest::spawn(temp_dir, root.clone(), &root);

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

    let hover_params = || {
        serde_json::json!({
            "textDocument": { "uri": &file_a_uri },
            "position": { "line": 0, "character": 0 }
        })
    };

    // First hover: forwarded to the backend and left pending on purpose.
    let cancelled_id = proxy
        .send_request("textDocument/hover", hover_params())
        .await;

    // Cancel it. The proxy has no pending fan-out or warmup queue entry for
    // this id, so it takes the "forward $/cancelRequest to backends" path.
    proxy
        .notify("$/cancelRequest", serde_json::json!({ "id": cancelled_id }))
        .await;

    // A fresh request on the same backend must still work normally: this
    // only succeeds if the mock progressed past its $/cancelRequest step,
    // proving the cancellation was actually forwarded.
    let hover2 = proxy.request("textDocument/hover", hover_params()).await;
    assert!(
        hover2.error.is_none(),
        "hover after cancellation should not return an error, got: {:?}",
        hover2.error
    );
    assert_eq!(
        hover2.result.as_ref().unwrap()["contents"]["value"],
        "hover after cancel"
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}
