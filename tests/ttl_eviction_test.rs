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
/// `evict_expired_backends` (`src/proxy/pool_management.rs:562-637`) treats
/// TTL expiry (`BackendPool::expired_venvs`, based on `last_used`) as
/// necessary but not sufficient: for each expired venv it also counts
/// `pending_requests` entries for that venv+session (client→backend,
/// ~line 585-599) and skips eviction while the count is > 0. `last_used` is
/// set when a request is FORWARDED to the backend (`forward_to_backend`),
/// not when its response arrives, so a slow in-flight request's age can
/// exceed the TTL well before it completes — exactly the window this test
/// drives through: the mock delays its response to the second hover by 3s
/// (`TYPEMUX_CC_POOL_SWEEP_INTERVAL=1` + `TYPEMUX_CC_BACKEND_TTL=1`), so the
/// sweep tick at t=2s finds this backend already past TTL, with the hover
/// still pending — a real race between the sweep and the response, not a
/// synthetic one. The sibling test below (`backend_awaiting_client_response_survives_ttl_sweep`)
/// covers the OTHER skip branch (`pending_backend_requests`, backend→client).
///
/// Single-lifetime scenario (no `write_venv_fixture` swap between hovers),
/// mirroring `cancel_request_test.rs`'s technique: if the pending-skip were
/// broken and the sweep evicted the backend anyway, the pending hover would
/// come back as the cancellation error the eviction path sends for it
/// instead of its scripted response — hover2's own successful, correctly-
/// valued response is sufficient proof by itself that the backend it was
/// forwarded to survived the sweep.
///
/// Deliberately no follow-up request after hover2: `last_used` only
/// refreshes at forward time (see above), so the instant hover2's response
/// arrives it goes stale again, and any further request here would race a
/// now-LEGITIMATE TTL eviction (the pending-skip condition no longer
/// applies once hover2 is answered) — a real flake, not a bug in the code
/// under test, and not something a longer sweep interval would fix either.
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

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}

/// E2E: a backend awaiting the CLIENT's response to one of ITS OWN
/// server-initiated requests (e.g. `workspace/configuration`) also survives
/// a TTL sweep — the mirror image of `backend_with_pending_request_survives_ttl_sweep`
/// above, which covers the client→backend direction.
///
/// `evict_expired_backends` (`src/proxy/pool_management.rs:562-637`) has two
/// independent skip branches. The sibling test above exercises the first
/// (`pending_requests`, client→backend). This test isolates the second
/// (`pending_backend_requests`, backend→client, ~line 601-615): a
/// server-initiated request forwarded to the client via
/// `forward_backend_request_to_client` (`backend_dispatch.rs:391`) stays
/// tracked in `pending_backend_requests` — keyed by a proxy-rewritten id —
/// until the client's response arrives and `dispatch_client_response`
/// (`client_dispatch.rs:163`) routes it back to the originating backend.
/// While that entry exists, `evict_expired_backends` must skip the backend
/// even though its TTL has expired.
///
/// Isolation from the first skip branch: the hover that carries the
/// `workspace/configuration` request is answered immediately (in the same
/// scripted step), so `pending_requests` is empty for the whole withholding
/// window below — survival here can only be explained by the second skip
/// branch, not the first.
///
/// The client (not the mock, unlike the sibling test) withholds its
/// response to `workspace/configuration` for 3s, spanning the sweep ticks
/// at t=1s/2s (`TYPEMUX_CC_POOL_SWEEP_INTERVAL=1` + `TYPEMUX_CC_BACKEND_TTL=1`)
/// while `last_used` (anchored at the carrying hover's forward time, unmoved
/// since — forwarding a server-initiated request to the client doesn't
/// touch it) is already past TTL. Up to that point `pending_requests` is
/// empty (both hovers already completed), so this whole window's survival
/// is attributable solely to the `pending_backend_requests` skip.
///
/// Right before finally answering, the client also sends a `didSave` for
/// the same document — routed via `forward_to_backend`
/// (`client_dispatch.rs`), it refreshes `last_used` without touching either
/// pending map. This doesn't relax what the 3s withholding window proves
/// (that window is over by the time `didSave` is sent); it exists because
/// `dispatch_client_response` removing the `pending_backend_requests` entry
/// isn't itself the end of the story — the backend (a real, separately
/// scheduled OS process) still needs to receive the routed-back response
/// and emit its marker, and an UNRELATED, perfectly legitimate sweep tick
/// firing in that gap (nothing pending anymore, TTL long expired) would
/// otherwise be a real, not synthetic, flake risk. `didSave` buys that
/// final observation a fresh TTL's worth of headroom.
///
/// Detection technique borrowed from `workspace_configuration_during_creating_is_answered_e2e`
/// (`creating_state_test.rs`, #134/#137): the mock only emits the
/// `window/logMessage` marker after successfully matching the routed-back
/// response as its next expected message (`"<response>"` step). If the
/// pending-skip were broken, eviction's fire-and-forget `shutdown`
/// request/`exit` notification would land on the mock instead, mismatching
/// that step and crashing it — `wait_for_notification` for the marker times
/// out instead of the response ever routing anywhere, since eviction's
/// unconditional `clean_pending_backend_requests` sweep already discarded
/// the `pending_backend_requests` entry before the client's (late) response
/// could use it.
#[tokio::test]
async fn backend_awaiting_client_response_survives_ttl_sweep() {
    let scenario = serde_json::json!({
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
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover baseline" } } }]
            },
            // Carries the server-initiated request AND answers immediately,
            // so pending_requests (client->backend) is empty for the rest of
            // this test — isolating the pending_backend_requests skip below.
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [
                    { "type": "request", "id": 42, "method": "workspace/configuration", "params": { "items": [{ "section": "python" }] } },
                    { "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover carrying config request" } } }
                ]
            },
            // Sent right before the withheld response, to refresh last_used
            // (see the doc comment above) — observed here, no action needed.
            { "expect": { "method": "textDocument/didSave" }, "actions": [] },
            // Only reached if the client's (late) response to
            // workspace/configuration was actually routed back here.
            {
                "expect": { "method": "<response>" },
                "actions": [
                    { "type": "notify", "method": "window/logMessage", "params": { "type": 3, "message": "config response received after sweep" } }
                ]
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

    // Second hover: the mock answers it right away but also fires a
    // server-initiated workspace/configuration request first, in the same
    // step. Both frames arrive in that order.
    let hover2_id = proxy
        .send_request("textDocument/hover", hover_params())
        .await;

    let config_req = proxy.read_next().await;
    assert_eq!(
        config_req.method.as_deref(),
        Some("workspace/configuration"),
        "expected the backend's server-initiated workspace/configuration request, got: {config_req:?}"
    );

    let hover2 = proxy.wait_for_response(hover2_id, 5000).await;
    assert!(
        hover2.error.is_none(),
        "hover carrying the server-initiated request should still succeed, got error: {:?}",
        hover2.error
    );

    // Withhold the client's response to workspace/configuration for 3s —
    // long enough to span the sweep ticks at t=1s and t=2s while this
    // backend's last_used (unmoved since hover2 was forwarded) is already
    // past the 1s TTL. pending_requests is already empty at this point (both
    // hovers above completed), so only the pending_backend_requests skip can
    // explain survival. The verification window this proves ends here.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // Refreshes last_used (see the doc comment above) right before finally
    // answering, so the backend has fresh headroom for the OS-scheduled mock
    // process to receive the response and emit its marker, undisturbed by an
    // unrelated (and by then legitimate) later sweep tick.
    proxy
        .notify(
            "textDocument/didSave",
            serde_json::json!({ "textDocument": { "uri": &file_a_uri } }),
        )
        .await;

    proxy
        .respond_to_backend_request(&config_req, serde_json::json!([{ "tabSize": 4 }]))
        .await;

    // Only arrives if the response was actually routed back to the SAME
    // backend session that sent the original request.
    let marker = proxy.wait_for_notification("window/logMessage", 5000).await;
    assert_eq!(
        marker.params.as_ref().unwrap()["message"],
        "config response received after sweep"
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}
