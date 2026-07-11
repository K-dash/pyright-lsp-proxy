mod support;

use support::{PackageConfig, ProxyUnderTest, WorkspaceConfig};

/// E2E: TTL eviction of an idle backend, observable in test time (issue
/// #126). Before the pool sweep tick was configurable, TTL eviction only
/// ran on a hardcoded 60s tick, making this untestable within a reasonable
/// wall-clock budget (#97, #124). `TYPEMUX_CC_POOL_SWEEP_INTERVAL=1` +
/// `TYPEMUX_CC_BACKEND_TTL=1` bring both down to 1s.
///
/// Eviction is observed two ways: the empty `publishDiagnostics` the proxy
/// sends for the evicted venv's open document (`wait_for_crash_cleanup`
/// reuses the same signal crash recovery uses), and — the stronger proof —
/// a second hover for the same file must go through a brand-new backend
/// lifetime (its own `initialize`/`didOpen` handshake), which only happens
/// if the first backend was actually evicted from the pool.
///
/// This is a proof-of-purpose test for the pool-sweep-interval knob, not
/// the full #97 TTL matrix (pending-request skip, etc.) — that's a
/// follow-up PR now that this is E2E-testable at all.
#[tokio::test]
async fn idle_backend_evicted_after_ttl_expiry() {
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
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover life1" } } }]
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
    let pkg_dir = root.join("pkg");
    let mut proxy = ProxyUnderTest::spawn_with_env(
        temp_dir,
        root.clone(),
        &pkg_dir,
        &[
            ("TYPEMUX_CC_POOL_SWEEP_INTERVAL", "1"),
            ("TYPEMUX_CC_BACKEND_TTL", "1"),
        ],
    );

    let root_uri = support::path_to_uri(&pkg_dir);
    let init_resp = proxy.initialize(&root_uri).await;
    assert!(
        init_resp.error.is_none(),
        "initialize should not return an error"
    );
    proxy.send_initialized().await;

    let file_a = pkg_dir.join("a.py");
    std::fs::write(&file_a, "a = 1\n").unwrap();
    let file_a_uri = support::path_to_uri(&file_a);
    proxy.did_open(&file_a_uri, "a = 1\n").await;

    let hover_params = serde_json::json!({
        "textDocument": { "uri": &file_a_uri },
        "position": { "line": 0, "character": 0 }
    });

    // Establishes doc.venv (so eviction later clears its diagnostics) and
    // sets last_used, the TTL clock's starting point.
    let hover1 = proxy
        .request("textDocument/hover", hover_params.clone())
        .await;
    assert!(
        hover1.error.is_none(),
        "hover before eviction should succeed, got error: {:?}",
        hover1.error
    );
    assert_eq!(
        hover1.result.as_ref().unwrap()["contents"]["value"],
        "hover life1"
    );

    // Idle from here: no further requests until the sweep evicts. Worst
    // case is ~1 sweep interval (1s) plus the 1s TTL itself; 8s is a
    // generous margin over that for CI jitter, while staying far below the
    // old hardcoded 60s tick.
    let notifications = proxy.wait_for_crash_cleanup(1, 8000).await;
    let diag_count = notifications
        .iter()
        .filter(|m| m.method.as_deref() == Some("textDocument/publishDiagnostics"))
        .count();
    assert!(
        diag_count >= 1,
        "expected at least 1 publishDiagnostics clearing a.py, got {diag_count}"
    );

    // Second lifetime: a lazy respawn on the next request restores a.py
    // (should_restore_document) and re-runs the initialize handshake. A
    // spurious extra respawn, or no eviction at all, would desync this
    // scenario's steps and fail the hover below.
    let scenario_life2 = serde_json::json!({
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
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover life2" } } }]
            },
            {
                "expect": { "method": "shutdown" },
                "actions": [{ "type": "respond", "body": null }]
            }
        ]
    });
    support::write_venv_fixture(&pkg_dir, &scenario_life2);

    let hover2 = proxy.request("textDocument/hover", hover_params).await;
    assert!(
        hover2.error.is_none(),
        "hover after TTL eviction should succeed against a respawned backend, got error: {:?}",
        hover2.error
    );
    assert_eq!(
        hover2.result.as_ref().unwrap()["contents"]["value"],
        "hover life2",
        "hover response must come from the respawned (life2) backend, proving the life1 backend was evicted"
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}
