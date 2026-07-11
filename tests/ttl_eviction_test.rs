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

/// E2E: a backend with a pending client request survives a TTL sweep, even
/// once its TTL has expired — the follow-up half of #97's TTL matrix that
/// `idle_backend_evicted_after_ttl_expiry` above left for this PR.
///
/// `evict_expired_backends` (src/proxy/pool_management.rs:522-589) treats
/// TTL expiry (`BackendPool::expired_venvs`, based on `last_used`) as
/// necessary but not sufficient: for each expired venv it also counts
/// `pending_requests` entries for that venv+session and skips eviction
/// while the count is > 0. `last_used` is set when a request is FORWARDED
/// to the backend (`forward_to_backend`), not when its response arrives, so
/// a slow in-flight request's age can exceed the TTL well before it
/// completes — exactly the window this test drives through: the mock
/// delays its response to the second hover by 3s
/// (`TYPEMUX_CC_POOL_SWEEP_INTERVAL=1` + `TYPEMUX_CC_BACKEND_TTL=1`), so the
/// sweep tick at t=2s finds this backend already past TTL, with the hover
/// still pending — a real race between the sweep and the response, not a
/// synthetic one.
///
/// Single-lifetime scenario (no `write_venv_fixture` swap between hovers),
/// mirroring `cancel_request_test.rs`'s technique: if the pending-skip were
/// broken and the sweep evicted the backend anyway, the pending hover would
/// come back as the cancellation error the eviction path sends for it
/// instead of its scripted response, and the third hover would hit a
/// respawned backend whose fresh mock process expects `initialize` first —
/// desyncing this scenario's step order (mismatched method, mock exits)
/// instead of matching the next scripted hover.
#[tokio::test]
async fn backend_with_pending_request_survives_ttl_sweep() {
    let scenario = serde_json::json!({
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
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover baseline" } } }]
            },
            // Left pending across at least one TTL-expired sweep tick before responding.
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [
                    { "type": "sleep_ms", "ms": 3000 },
                    { "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover survived sweep" } } }
                ]
            },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover same backend" } } }]
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

    let hover_params = || {
        serde_json::json!({
            "textDocument": { "uri": &file_a_uri },
            "position": { "line": 0, "character": 0 }
        })
    };

    // Establishes doc.venv and sets last_used, the TTL clock's starting point.
    let hover1 = proxy.request("textDocument/hover", hover_params()).await;
    assert!(
        hover1.error.is_none(),
        "baseline hover should succeed, got error: {:?}",
        hover1.error
    );

    // Left pending while the sweep tick at t=2s finds this backend already
    // past its 1s TTL (last_used was set when this hover was forwarded, at
    // t≈0). The response only arrives ~3s later, well after that tick ran.
    // Sent via `send_request`/`wait_for_response` (not `request`'s bare
    // `read_next`, whose fixed 5s timeout leaves only ~2s of margin over the
    // 3s mock delay) with an explicit 8s margin, matching
    // `idle_backend_evicted_after_ttl_expiry`'s documented-margin style.
    let hover2_id = proxy
        .send_request("textDocument/hover", hover_params())
        .await;
    let hover2 = proxy.wait_for_response(hover2_id, 8000).await;
    assert!(
        hover2.error.is_none(),
        "hover with a request pending across a TTL sweep should still succeed, got error: {:?}",
        hover2.error
    );
    assert_eq!(
        hover2.result.as_ref().unwrap()["contents"]["value"],
        "hover survived sweep",
        "response must come from the SAME backend life that received the request \
         before the sweep ran — a cancellation error or a respawned backend's \
         response both indicate the pending-request skip failed"
    );

    // Same backend, no respawn: the mock's single-lifetime scenario has only
    // one `initialize` step, so this would desync (mismatched method, mock
    // exits) had the backend actually been evicted and lazily respawned.
    let hover3 = proxy.request("textDocument/hover", hover_params()).await;
    assert!(
        hover3.error.is_none(),
        "hover after the pending request should still succeed on the same backend, got error: {:?}",
        hover3.error
    );
    assert_eq!(
        hover3.result.as_ref().unwrap()["contents"]["value"],
        "hover same backend"
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}
