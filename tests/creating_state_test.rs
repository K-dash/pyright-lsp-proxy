mod support;

use support::{PackageConfig, ProxyUnderTest, WorkspaceConfig};
use typemux_cc::message::RpcId;

/// Mirrors `MAX_CREATING_QUEUE_LEN` in `src/backend_pool.rs`. Not exported
/// from the lib crate (the E2E harness only links `framing`/`message` and
/// drives the proxy as a black-box subprocess), so it is duplicated here.
const MAX_CREATING_QUEUE_LEN: usize = 64;

/// Burst size for `restoration_diagnostics_reach_client_during_creating_e2e`.
/// Large enough to give the reader task a realistic chance of forwarding at
/// least some messages while the venv is still genuinely `Creating` (the
/// completion handler can't run until the burst stops arriving on
/// `backend_msg_rx` — `biased;` always prefers draining it), though this
/// isn't airtight: see that test's doc comment on why the deterministic
/// detection-power coverage for the currency-check fix lives in
/// `backend_dispatch.rs`'s unit tests instead of here.
const DIAGNOSTICS_BURST_LEN: usize = 500;

/// E2E (#93): a venv respawn whose restoration writes a document that
/// triggers a diagnostics burst well past the `backend_msg_rx` channel
/// capacity (1024) completes without hanging the proxy.
///
/// A burst under 1024 can't distinguish this fix from a naive "split before
/// restore, but still run restoration inline on the main loop" partial fix:
/// with only the OS pipe (~64KB) as backpressure, the main loop's brief
/// window of inline work (venv-token stat, pool insert) usually returns to
/// draining `backend_msg_rx` before the pipe fills for a single document.
/// Restoring TWO documents, with the first triggering the burst before the
/// second is written, is what actually reproduces the deadlock precondition
/// under a naive fix: the reader task blocks on the full 1024-capacity
/// channel, the OS pipe backs up behind it, the mock's write blocks, and if
/// restoration (including the second document's write) runs on the SAME
/// task as the main loop, that task is now stuck too — nobody is left to
/// drain the channel and unblock the reader. This fix keeps restoration on
/// an independent `tokio::spawn`ed task, so the main loop always keeps
/// draining `backend_msg_rx` regardless of what the creation task is doing.
#[tokio::test]
// `file_a`/`file_b` (and their `_uri` variants) are deliberately parallel
// names for the two test fixtures this scenario exercises.
#[allow(clippy::similar_names)]
async fn restoration_survives_large_diagnostics_burst_e2e() {
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
        &[("TYPEMUX_CC_VENV_CHECK_INTERVAL", "1")],
    );

    let root_uri = support::path_to_uri(&pkg_dir);
    let init_resp = proxy.initialize(&root_uri).await;
    assert!(
        init_resp.error.is_none(),
        "initialize should not return an error"
    );
    proxy.send_initialized().await;

    // Two open documents under the same venv: restoration on respawn will
    // write both.
    let file_a = pkg_dir.join("a.py");
    std::fs::write(&file_a, "a = 1\n").unwrap();
    let file_a_uri = support::path_to_uri(&file_a);
    proxy.did_open(&file_a_uri, "a = 1\n").await;

    let file_b = pkg_dir.join("b.py");
    std::fs::write(&file_b, "b = 2\n").unwrap();
    let file_b_uri = support::path_to_uri(&file_b);
    proxy.did_open(&file_b_uri, "b = 2\n").await;

    let hover_params = || {
        serde_json::json!({
            "textDocument": { "uri": &file_a_uri },
            "position": { "line": 0, "character": 0 }
        })
    };

    let hover1 = proxy.request("textDocument/hover", hover_params()).await;
    assert!(
        hover1.error.is_none(),
        "hover life1 failed: {:?}",
        hover1.error
    );
    assert_eq!(
        hover1.result.as_ref().unwrap()["contents"]["value"],
        "hover life1"
    );

    // Simulate `uv sync`: delete and recreate `.venv`, past the 1s debounce.
    std::fs::remove_dir_all(pkg_dir.join(".venv")).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

    // Life2: restoring a.py triggers a >1024-notification burst BEFORE the
    // mock reads b.py's restoration didOpen. A programmatically generated
    // burst, not hand-written, to make the exact count trivially adjustable.
    let burst_actions: Vec<serde_json::Value> = (0..1025)
        .map(|i| {
            serde_json::json!({
                "type": "notify",
                "method": "textDocument/publishDiagnostics",
                "params": {
                    "uri": format!("file:///proj/burst_{i}.py"),
                    "diagnostics": []
                }
            })
        })
        .collect();

    let scenario_life2 = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [{ "type": "respond", "body": { "capabilities": { "hoverProvider": true } } }]
            },
            { "expect": { "method": "initialized" }, "actions": [] },
            { "expect": { "method": "textDocument/didOpen" }, "actions": burst_actions },
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
    std::fs::write(
        pkg_dir.join(".venv/pyvenv.cfg"),
        "home = /usr/bin\nversion = 3.12\n",
    )
    .unwrap();

    // Triggers the staleness check -> Mismatch -> respawn -> restoration of
    // both a.py and b.py, racing the burst. The harness's 5s read timeout is
    // the deadlock detector: if restoration ever blocks the main loop, this
    // panics with a timeout instead of returning a response.
    let hover2 = proxy.request("textDocument/hover", hover_params()).await;
    assert!(
        hover2.error.is_none(),
        "hover life2 (after burst) failed: {:?}",
        hover2.error
    );
    assert_eq!(
        hover2.result.as_ref().unwrap()["contents"]["value"],
        "hover life2"
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}

/// E2E (#96 AC1): with venv A warm, a request to it completes at normal
/// latency while venv B's backend is mid-handshake (slow `initialize`
/// response). Detects: cold spawn / respawn no longer blocks the whole
/// event loop.
#[tokio::test]
// `venv-a`/`venv-b` (and their `hover_a`/`hover_b`/`file_a`/`file_b` variants)
// are deliberately parallel names for the two test fixtures this scenario
// exercises.
#[allow(clippy::similar_names)]
async fn cold_spawn_does_not_block_warm_venv_e2e() {
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
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover a sync" } } }]
            },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover a during b handshake" } } }]
            },
            {
                "expect": { "method": "shutdown" },
                "actions": [{ "type": "respond", "body": null }]
            }
        ]
    });

    // Handshake deliberately slow: 2s, far longer than the margin asserted
    // on venv A's second hover below.
    let scenario_b = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [
                    { "type": "sleep_ms", "ms": 2000 },
                    { "type": "respond", "body": { "capabilities": { "hoverProvider": true } } }
                ]
            },
            { "expect": { "method": "initialized" }, "actions": [] },
            { "expect": { "method": "textDocument/didOpen" }, "actions": [] },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover b" } } }]
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
                name: "venv-a".to_string(),
                scenario: scenario_a,
                has_venv: true,
            },
            PackageConfig {
                name: "venv-b".to_string(),
                scenario: scenario_b,
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

    let file_a = root.join("venv-a/main.py");
    std::fs::write(&file_a, "a = 1\n").unwrap();
    let file_a_uri = support::path_to_uri(&file_a);
    proxy.did_open(&file_a_uri, "a = 1\n").await;

    let hover_a_params = || {
        serde_json::json!({
            "textDocument": { "uri": &file_a_uri },
            "position": { "line": 0, "character": 0 }
        })
    };

    // Synchronizing round-trip: venv A is fully Ready before B's slow spawn starts.
    let sync_a = proxy.request("textDocument/hover", hover_a_params()).await;
    assert!(
        sync_a.error.is_none(),
        "sync hover(a) failed: {:?}",
        sync_a.error
    );
    assert_eq!(
        sync_a.result.as_ref().unwrap()["contents"]["value"],
        "hover a sync"
    );

    // Kick off venv B's slow cold spawn (fire-and-forget).
    let file_b = root.join("venv-b/main.py");
    std::fs::write(&file_b, "b = 2\n").unwrap();
    let file_b_uri = support::path_to_uri(&file_b);
    proxy.did_open(&file_b_uri, "b = 2\n").await;

    // Measured while B is mid-handshake: must return well under B's 2s
    // delay if A's traffic isn't blocked by B's cold spawn.
    let start = std::time::Instant::now();
    let hover_a2 = proxy.request("textDocument/hover", hover_a_params()).await;
    let elapsed = start.elapsed();
    assert!(
        hover_a2.error.is_none(),
        "hover(a) during b's handshake failed: {:?}",
        hover_a2.error
    );
    assert_eq!(
        hover_a2.result.as_ref().unwrap()["contents"]["value"],
        "hover a during b handshake"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "hover(a) took {elapsed:?} while venv B was mid-handshake — the event loop was blocked"
    );

    // B eventually completes creation and serves normally.
    let hover_b_params = serde_json::json!({
        "textDocument": { "uri": &file_b_uri },
        "position": { "line": 0, "character": 0 }
    });
    let hover_b = proxy.request("textDocument/hover", hover_b_params).await;
    assert!(
        hover_b.error.is_none(),
        "hover(b) failed: {:?}",
        hover_b.error
    );
    assert_eq!(
        hover_b.result.as_ref().unwrap()["contents"]["value"],
        "hover b"
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}

/// E2E (#96 AC2): two un-awaited requests for the same cold venv spawn
/// exactly one backend. The mock scenario has a single `initialize` step; a
/// double-spawn sends it a second `initialize` where it expects the first
/// hover, crashing the mock — a loud, detectable failure.
#[tokio::test]
async fn concurrent_requests_same_cold_venv_spawn_once_e2e() {
    let scenario = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [{ "type": "respond", "body": { "capabilities": { "hoverProvider": true } } }]
            },
            { "expect": { "method": "initialized" }, "actions": [] },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover1" } } }]
            },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover2" } } }]
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

    // No didOpen: both requests resolve the venv fresh via find_venv, never
    // touching the didOpen-caches-then-creates path.
    let hover_params = serde_json::json!({
        "textDocument": { "uri": support::path_to_uri(&root.join("pkg/main.py")) },
        "position": { "line": 0, "character": 0 }
    });

    // Both un-awaited: the first starts creation, the second must see it
    // already in flight and queue rather than spawn a second backend.
    let id1 = proxy
        .send_request("textDocument/hover", hover_params.clone())
        .await;
    let id2 = proxy.send_request("textDocument/hover", hover_params).await;

    // Replay is FIFO, so responses arrive in request order.
    let resp1 = proxy.read_next().await;
    assert_eq!(
        resp1.id,
        Some(RpcId::Number(id1)),
        "unexpected response order: {resp1:?}"
    );
    assert!(resp1.error.is_none(), "hover1 failed: {:?}", resp1.error);
    assert_eq!(
        resp1.result.as_ref().unwrap()["contents"]["value"],
        "hover1"
    );

    let resp2 = proxy.read_next().await;
    assert_eq!(
        resp2.id,
        Some(RpcId::Number(id2)),
        "unexpected response order: {resp2:?}"
    );
    assert!(resp2.error.is_none(), "hover2 failed: {:?}", resp2.error);
    assert_eq!(
        resp2.result.as_ref().unwrap()["contents"]["value"],
        "hover2"
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}

/// E2E (#96 AC3, fast-failure path / #92 containment): a broken venv's
/// backend-creation failure is contained — the queued request against it
/// gets an explicit JSON-RPC error, and a concurrent request to another,
/// healthy venv completes normally.
#[tokio::test]
async fn creating_backend_failure_does_not_stall_other_venvs_e2e() {
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

    let config = WorkspaceConfig {
        packages: vec![
            PackageConfig {
                name: "good".to_string(),
                scenario: scenario_good,
                has_venv: true,
            },
            PackageConfig {
                name: "broken".to_string(),
                scenario: serde_json::json!({}),
                has_venv: false,
            },
        ],
    };

    let (temp_dir, root) = support::setup_test_workspace(&config);
    // Broken venv: pyvenv.cfg present but the shim exits immediately, so the
    // handshake fails deterministically and fast.
    support::write_broken_venv_fixture(&root.join("broken"));

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

    // Un-awaited: triggers creation for the broken venv, which will fail.
    let broken_hover_params = serde_json::json!({
        "textDocument": { "uri": support::path_to_uri(&root.join("broken/main.py")) },
        "position": { "line": 0, "character": 0 }
    });
    let broken_id = proxy
        .send_request("textDocument/hover", broken_hover_params)
        .await;

    // The good venv must stay responsive regardless of the broken venv's
    // outcome — read this first since it doesn't depend on the broken
    // venv's (unordered relative to this) failure notification.
    let hover_good2 = proxy
        .request("textDocument/hover", hover_good_params())
        .await;
    assert!(
        hover_good2.error.is_none(),
        "hover(good) after broken creation started failed: {:?}",
        hover_good2.error
    );
    assert_eq!(
        hover_good2.result.as_ref().unwrap()["contents"]["value"],
        "hover good again"
    );

    // The broken venv's queued request must get an explicit error — not silence.
    let broken_resp = proxy.wait_for_response(broken_id, 5000).await;
    let error = broken_resp
        .error
        .expect("broken venv's queued hover must fail explicitly");
    assert!(
        error.message.contains("backend error"),
        "expected a backend error message, got: {}",
        error.message
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}

/// E2E (#96 AC3, timeout path): a handshake that never completes is bounded
/// by `TYPEMUX_CC_INIT_HANDSHAKE_TIMEOUT`; other venvs stay responsive
/// throughout, and the queued request against the wedged venv gets a JSON-RPC
/// error once the timeout fires.
#[tokio::test]
async fn handshake_timeout_does_not_stall_other_venvs_e2e() {
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

    // Never answers initialize.
    let scenario_wedged = serde_json::json!({
        "on_startup": [],
        "steps": [
            { "expect": { "method": "initialize" }, "actions": [] }
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
                name: "wedged".to_string(),
                scenario: scenario_wedged,
                has_venv: true,
            },
        ],
    };

    let (temp_dir, root) = support::setup_test_workspace(&config);
    let mut proxy = ProxyUnderTest::spawn_with_env(
        temp_dir,
        root.clone(),
        &root,
        &[("TYPEMUX_CC_INIT_HANDSHAKE_TIMEOUT", "1")],
    );

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

    // Un-awaited: triggers creation for the wedged venv.
    let wedged_hover_params = serde_json::json!({
        "textDocument": { "uri": support::path_to_uri(&root.join("wedged/main.py")) },
        "position": { "line": 0, "character": 0 }
    });
    let wedged_id = proxy
        .send_request("textDocument/hover", wedged_hover_params)
        .await;

    // Measured immediately: must return well under the 1s handshake timeout
    // if the good venv isn't blocked by the wedged one.
    let start = std::time::Instant::now();
    let hover_good2 = proxy
        .request("textDocument/hover", hover_good_params())
        .await;
    let elapsed = start.elapsed();
    assert!(
        hover_good2.error.is_none(),
        "hover(good) during wedged handshake failed: {:?}",
        hover_good2.error
    );
    assert!(
        elapsed < std::time::Duration::from_millis(700),
        "hover(good) took {elapsed:?} while the wedged venv was mid-handshake — the event loop was blocked"
    );

    // The wedged venv's queued request must fail once the 1s timeout fires.
    // The harness's 5s read timeout gives ample margin over that.
    let wedged_resp = proxy.wait_for_response(wedged_id, 5000).await;
    let error = wedged_resp
        .error
        .expect("wedged venv's queued hover must fail explicitly after the handshake timeout");
    assert!(
        error.message.contains("backend error"),
        "expected a backend error message, got: {}",
        error.message
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}

/// E2E: a second `didOpen` (different file, same venv) landing while the
/// first is still `Creating` (mid-handshake) is queued rather than dropped,
/// and delivered once creation completes — both documents are hoverable
/// afterward.
#[tokio::test]
// `file_a`/`file_b` (and their `_uri` variants) are deliberately parallel
// names for the two test fixtures this scenario exercises.
#[allow(clippy::similar_names)]
async fn didopen_during_creating_is_queued_and_delivered_e2e() {
    let scenario = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [
                    { "type": "sleep_ms", "ms": 300 },
                    { "type": "respond", "body": { "capabilities": { "hoverProvider": true } } }
                ]
            },
            { "expect": { "method": "initialized" }, "actions": [] },
            // a.py: restored from the snapshot taken when its didOpen started creation.
            { "expect": { "method": "textDocument/didOpen" }, "actions": [] },
            // b.py: queued while Creating, delivered via replay after completion.
            { "expect": { "method": "textDocument/didOpen" }, "actions": [] },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover a" } } }]
            },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover b" } } }]
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
    // Starts creation (300ms handshake delay); a.py is captured in the
    // restoration snapshot this call takes.
    proxy.did_open(&file_a_uri, "a = 1\n").await;

    let file_b = root.join("pkg/b.py");
    std::fs::write(&file_b, "b = 2\n").unwrap();
    let file_b_uri = support::path_to_uri(&file_b);
    // Lands well within the 300ms handshake window: creation is already in
    // flight, so this queues instead of being in the snapshot.
    proxy.did_open(&file_b_uri, "b = 2\n").await;

    let hover_a = proxy
        .request(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": &file_a_uri },
                "position": { "line": 0, "character": 0 }
            }),
        )
        .await;
    assert!(
        hover_a.error.is_none(),
        "hover(a) failed: {:?}",
        hover_a.error
    );
    assert_eq!(
        hover_a.result.as_ref().unwrap()["contents"]["value"],
        "hover a"
    );

    let hover_b = proxy
        .request(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": &file_b_uri },
                "position": { "line": 0, "character": 0 }
            }),
        )
        .await;
    assert!(
        hover_b.error.is_none(),
        "hover(b) failed: {:?}",
        hover_b.error
    );
    assert_eq!(
        hover_b.result.as_ref().unwrap()["contents"]["value"],
        "hover b"
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}

/// E2E: a request queued while its venv is `Creating` is removed by
/// `$/cancelRequest` and never delivered after replay.
///
/// Like `cancel_request_test.rs`, the mock DSL can only match by method
/// name, so it can't fabricate a response tied to the cancelled request's
/// id from a later step. This test proves the contract observable through
/// the DSL: the scenario scripts exactly ONE `hover` step. If cancellation
/// failed, the backend would see two hovers — the cancelled one (consuming
/// the sole scripted step) and the verifying one (falling into the mock's
/// drain loop as an unexpected message, crashing it) — so the verifying
/// hover would fail or hang instead of returning its scripted value.
#[tokio::test]
async fn cancel_request_during_creating_e2e() {
    let scenario = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [
                    { "type": "sleep_ms", "ms": 300 },
                    { "type": "respond", "body": { "capabilities": { "hoverProvider": true } } }
                ]
            },
            { "expect": { "method": "initialized" }, "actions": [] },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "verify hover" } } }]
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

    let hover_params = serde_json::json!({
        "textDocument": { "uri": support::path_to_uri(&root.join("pkg/main.py")) },
        "position": { "line": 0, "character": 0 }
    });

    // Un-awaited: starts creation (300ms handshake delay) and queues.
    let cancelled_id = proxy
        .send_request("textDocument/hover", hover_params.clone())
        .await;

    // Cancelled immediately, well within the 300ms window: removed from the
    // Creating queue before it can ever reach the backend.
    proxy
        .notify("$/cancelRequest", serde_json::json!({ "id": cancelled_id }))
        .await;

    // This one is NOT queued behind the cancelled one at the JSON-RPC id
    // level, but arrives after the cancellation and, once the backend is
    // Ready, is the only hover the mock ever sees.
    let verify = proxy.request("textDocument/hover", hover_params).await;
    assert!(
        verify.error.is_none(),
        "verifying hover failed: {:?}",
        verify.error
    );
    assert_eq!(
        verify.result.as_ref().unwrap()["contents"]["value"],
        "verify hover"
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}

/// E2E: the 65th request queued for a `Creating` venv is rejected
/// immediately with a JSON-RPC error instead of being queued.
///
/// The handshake is deliberately slow (2s) so the backend stays `Creating`
/// for the whole test — the queue can only shrink via the capacity
/// rejection under test, never via replay draining it (mirrors
/// `warmup_queue_limit_test.rs`'s discipline for the analogous warmup queue).
#[tokio::test]
async fn creating_queue_full_rejects_e2e() {
    let scenario = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [
                    { "type": "sleep_ms", "ms": 2000 },
                    { "type": "respond", "body": { "capabilities": { "hoverProvider": true } } }
                ]
            },
            { "expect": { "method": "initialized" }, "actions": [] }
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

    let hover_params = || {
        serde_json::json!({
            "textDocument": { "uri": support::path_to_uri(&root.join("pkg/main.py")) },
            "position": { "line": 0, "character": 0 }
        })
    };

    // First request starts creation (2s handshake); the rest queue behind it.
    for _ in 0..MAX_CREATING_QUEUE_LEN {
        proxy
            .send_request("textDocument/hover", hover_params())
            .await;
    }

    // The next one overflows the queue and must be rejected immediately.
    let overflow_id = proxy
        .send_request("textDocument/hover", hover_params())
        .await;

    let resp = proxy.read_next().await;
    assert_eq!(
        resp.id,
        Some(RpcId::Number(overflow_id)),
        "expected an immediate response for the overflowing request, got {resp:?}"
    );
    assert!(
        resp.error.is_some(),
        "overflowing request should get a JSON-RPC error response, got {resp:?}"
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}

/// E2E (#93 acceptance criterion, regression coverage): a backend's own
/// reaction to a restoration `didOpen` (e.g. `publishDiagnostics`) — sent
/// while the venv is still `Creating`, since the reader task already drains
/// it at that point — still reaches the client instead of being discarded
/// as a stale message.
///
/// `dispatch_backend_message`'s currency check used to recognize only the
/// `Ready` `backends` map; a message arriving during the Creating window
/// (which the reader task can legitimately produce, by design of the #93
/// fix) matched nothing there and was silently dropped. #93 explicitly
/// requires "no message loss: early diagnostics from the restoration window
/// still reach the client."
///
/// Uses a burst (not a single notification) to make this deterministic
/// rather than racy: a lone notification can lose the race against the
/// creation task's own remaining work (venv-token stat, channel send) and
/// land after the completion handler has already inserted the instance —
/// at which point it's genuinely `Ready` and would be delivered correctly
/// either way, proving nothing about the Creating-window path specifically.
/// `biased;` in the main select! always prefers draining `backend_msg_rx`
/// over processing `creation_rx`, so as long as the reader task keeps
/// producing burst messages, the completion handler (and thus the pool
/// insert) can't run — every message in the burst is forced through the
/// Creating-window path, not just whichever one happens to win a one-shot
/// race. Verified empirically: a single notification here does NOT reliably
/// distinguish the fix (see the report).
#[tokio::test]
async fn restoration_diagnostics_reach_client_during_creating_e2e() {
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
        &[("TYPEMUX_CC_VENV_CHECK_INTERVAL", "1")],
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

    let hover1 = proxy.request("textDocument/hover", hover_params()).await;
    assert!(
        hover1.error.is_none(),
        "hover life1 failed: {:?}",
        hover1.error
    );

    // Simulate `uv sync`: delete and recreate `.venv`, past the 1s debounce.
    std::fs::remove_dir_all(pkg_dir.join(".venv")).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

    // Life2: the mock emits a burst of publishDiagnostics as its reaction to
    // the restoration didOpen — i.e. while the venv is still Creating (see
    // the doc comment above for why a burst, not a single notification).
    let burst_actions: Vec<serde_json::Value> = (0..DIAGNOSTICS_BURST_LEN)
        .map(|i| {
            serde_json::json!({
                "type": "notify",
                "method": "textDocument/publishDiagnostics",
                "params": {
                    "uri": &file_a_uri,
                    "diagnostics": [{
                        "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } },
                        "severity": 1,
                        "message": format!("restoration diagnostic {i}")
                    }]
                }
            })
        })
        .collect();

    let scenario_life2 = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [{ "type": "respond", "body": { "capabilities": { "hoverProvider": true } } }]
            },
            { "expect": { "method": "initialized" }, "actions": [] },
            { "expect": { "method": "textDocument/didOpen" }, "actions": burst_actions },
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
    std::fs::write(
        pkg_dir.join(".venv/pyvenv.cfg"),
        "home = /usr/bin\nversion = 3.12\n",
    )
    .unwrap();

    // Triggers the staleness check -> Mismatch -> respawn; this hover
    // request itself queues behind the in-flight creation and is replayed
    // once Ready. `request_collecting` captures notifications observed
    // while waiting for the response — including the diagnostics published
    // during the Creating window, before this request was even replayed.
    let (hover2, notifications) = proxy
        .request_collecting("textDocument/hover", hover_params())
        .await;
    assert!(
        hover2.error.is_none(),
        "hover life2 failed: {:?}",
        hover2.error
    );
    assert_eq!(
        hover2.result.as_ref().unwrap()["contents"]["value"],
        "hover life2"
    );

    // Excludes empty-diagnostics `publishDiagnostics`: evicting the life1
    // backend (as part of the Mismatch respawn, before life2's creation even
    // starts) clears diagnostics for every doc under the venv, which is a
    // legitimate, unrelated `publishDiagnostics(uri, [])` for this same URI.
    let diagnostics_received = notifications
        .iter()
        .filter(|n| {
            n.method.as_deref() == Some("textDocument/publishDiagnostics")
                && n.params.as_ref().and_then(|p| p["uri"].as_str()) == Some(file_a_uri.as_str())
                && n.params
                    .as_ref()
                    .and_then(|p| p["diagnostics"].as_array())
                    .is_some_and(|d| !d.is_empty())
        })
        .count();
    assert_eq!(
        diagnostics_received, DIAGNOSTICS_BURST_LEN,
        "expected all {DIAGNOSTICS_BURST_LEN} restoration-triggered diagnostics to reach the client, got {diagnostics_received}"
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}

/// E2E (zombie-pooling regression coverage): a backend that dies during
/// creation — after the handshake, while restoration is in flight — must
/// never leave the queued request against it hanging. It gets an explicit
/// error either way, through one of two race-adjacent paths:
///
/// - `handle_creation_outcome`'s liveness check (`child.try_wait()`) sees
///   the death before inserting into `backends`, and fails the creation.
/// - Or the zombie is inserted a moment before the reader task's EOF is
///   processed, in which case it's `Ready` with a matching session by then,
///   and the ordinary crash-cleanup path (pre-existing, not new) catches it
///   immediately after — same observable result, different message text.
///
/// The mock crashes right after reading the restoration `didOpen` — a
/// fire-and-forget notification, so the proxy's write of it still succeeds
/// (the crash races the mock's own read-then-exit, not the proxy's write),
/// which is what makes this a genuine race between the two paths above
/// rather than a deterministic hit on the liveness check alone. Both are
/// "creation-time death was contained, nothing hung" — the property this
/// test asserts. The write-failure path (Layer 1: a document write itself
/// failing) is covered separately and deterministically by a lower-level
/// test in `initialization.rs`, which isn't subject to this OS-timing race.
#[tokio::test]
async fn dead_backend_during_creation_does_not_pool_as_zombie_e2e() {
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

    // Dies right after accepting the restoration didOpen — after the
    // handshake completes (so this is a creation-phase, not spawn/handshake,
    // failure), before ever responding to a hover.
    let scenario_dying = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [{ "type": "respond", "body": { "capabilities": { "hoverProvider": true } } }]
            },
            { "expect": { "method": "initialized" }, "actions": [] },
            { "expect": { "method": "textDocument/didOpen" }, "actions": [{ "type": "crash" }] }
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
                name: "dying".to_string(),
                scenario: scenario_dying,
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

    // Opening a document under "dying" starts its creation; restoration
    // writes this didOpen back to it during the handshake-then-restore
    // window, and the mock exits right after reading it.
    let file_dying = root.join("dying/main.py");
    std::fs::write(&file_dying, "d = 1\n").unwrap();
    let file_dying_uri = support::path_to_uri(&file_dying);
    proxy.did_open(&file_dying_uri, "d = 1\n").await;

    // Un-awaited: queues behind the in-flight (and soon-to-be-dead) creation.
    let dying_hover_params = serde_json::json!({
        "textDocument": { "uri": &file_dying_uri },
        "position": { "line": 0, "character": 0 }
    });
    let dying_id = proxy
        .send_request("textDocument/hover", dying_hover_params)
        .await;

    // The good venv must stay unaffected.
    let hover_good2 = proxy
        .request("textDocument/hover", hover_good_params())
        .await;
    assert!(
        hover_good2.error.is_none(),
        "hover(good) failed: {:?}",
        hover_good2.error
    );
    assert_eq!(
        hover_good2.result.as_ref().unwrap()["contents"]["value"],
        "hover good again"
    );

    // The dying venv's queued request must get an explicit error — not a
    // hang (which is what forwarding into a zombie's dead pipe would cause).
    // Either race path (see the doc comment above) produces a distinct but
    // equally explicit message: the liveness check's creation-failure path
    // says "backend error"; the pre-existing crash-cleanup path (if the
    // zombie was briefly Ready) says "cancelled due to backend eviction".
    let dying_resp = proxy.wait_for_response(dying_id, 5000).await;
    let error = dying_resp
        .error
        .expect("queued hover against a backend that died during creation must fail explicitly");
    assert!(
        error.message.contains("backend error") || error.message.contains("cancelled"),
        "expected an explicit backend-death error, got: {}",
        error.message
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}
