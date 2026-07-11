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

/// E2E (blocking review finding): a `didClose` landing while its venv is
/// still `Creating` must still reach the backend, in order, after the
/// restoration snapshot's replay — not be silently dropped.
///
/// The restoration snapshot is taken when the didOpen starts creation,
/// before this didClose can possibly land, so the creation task always
/// replays the didOpen regardless. If the didClose were dropped instead of
/// queued, the backend would permanently hold a document the proxy already
/// considers closed. The mock's strictly-ordered scenario (didOpen, THEN
/// didClose, THEN a verifying hover) makes this observable: if didClose
/// isn't delivered, the mock receives the verifying hover while still
/// expecting didClose, mismatches, and crashes — so the verifying hover
/// fails instead of returning its scripted value.
#[tokio::test]
async fn didclose_during_creating_reaches_backend_e2e() {
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
            { "expect": { "method": "textDocument/didOpen" }, "actions": [] },
            { "expect": { "method": "textDocument/didClose" }, "actions": [] },
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

    let file_a = root.join("pkg/a.py");
    std::fs::write(&file_a, "a = 1\n").unwrap();
    let file_a_uri = support::path_to_uri(&file_a);
    // Starts creation (300ms handshake delay); a.py is captured in the
    // restoration snapshot this call takes.
    proxy.did_open(&file_a_uri, "a = 1\n").await;

    // Lands well within the 300ms handshake window: creation is already in
    // flight, so this must queue instead of being silently dropped.
    proxy
        .notify(
            "textDocument/didClose",
            serde_json::json!({ "textDocument": { "uri": &file_a_uri } }),
        )
        .await;

    // Only succeeds if the mock progressed past its didClose step, proving
    // the didClose was actually delivered (queued and replayed) in order.
    let verify = proxy
        .request(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": &file_a_uri },
                "position": { "line": 0, "character": 0 }
            }),
        )
        .await;
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

/// E2E (blocking review finding, round 3): a `didClose` queued behind a
/// creation that then FAILS must still update the document cache before
/// the venv's next (lazy-retry) creation attempt — not be dropped along
/// with the queued requests.
///
/// Round 2 deferred a queued didClose's cache removal to whichever pass
/// resolves it, on the assumption that pass is always a replay. Creation
/// failure has no replay for notifications by default (only queued
/// *requests* get an explicit error response), so without also replaying
/// queued notifications on failure, the deferred removal never runs: the
/// closed document stays cached, and the next creation's restoration
/// snapshot resurrects it as a ghost `didOpen` the new mock never expects.
///
/// This mock's second scenario is strictly ordered with exactly one
/// `textDocument/didOpen` step (the still-open document). A ghost replay
/// of the closed document's `didOpen` desyncs the scenario against the
/// next step (`textDocument/hover`) and crashes the mock — so the
/// verifying hover times out instead of returning its scripted value.
#[tokio::test]
async fn didclose_queued_behind_failing_creation_prevents_ghost_replay_e2e() {
    // Creation 1: dies mid-handshake (after reading `initialize`, before
    // responding), giving a real — if brief — Creating window to queue the
    // didClose against, then failing deterministically.
    let scenario_crash = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [
                    { "type": "sleep_ms", "ms": 300 },
                    { "type": "crash" }
                ]
            }
        ]
    });

    let config = WorkspaceConfig {
        packages: vec![PackageConfig {
            name: "pkg".to_string(),
            scenario: scenario_crash,
            has_venv: true,
        }],
    };

    let (temp_dir, root) = support::setup_test_workspace(&config);
    let pkg_dir = root.join("pkg");
    let mut proxy = ProxyUnderTest::spawn(temp_dir, root.clone(), &root);

    let root_uri = support::path_to_uri(&root);
    let init_resp = proxy.initialize(&root_uri).await;
    assert!(
        init_resp.error.is_none(),
        "initialize should not return an error"
    );
    proxy.send_initialized().await;

    let closed_doc = pkg_dir.join("a.py");
    std::fs::write(&closed_doc, "a = 1\n").unwrap();
    let closed_doc_uri = support::path_to_uri(&closed_doc);
    // Starts creation 1 (300ms handshake delay before it crashes).
    proxy.did_open(&closed_doc_uri, "a = 1\n").await;

    // Lands well within the 300ms window: creation 1 is in flight, so this
    // queues behind it instead of forwarding anywhere.
    proxy
        .notify(
            "textDocument/didClose",
            serde_json::json!({ "textDocument": { "uri": &closed_doc_uri } }),
        )
        .await;

    // Synchronizing round-trip: creation 1's failure notification is only
    // sent after `creating_remove` and the (fixed) requeue of the queued
    // didClose into `replay_queue` have already happened — and, just as
    // importantly for this test, only after creation 1's own process has
    // already spawned and read whatever scenario file was on disk at that
    // time. Repairing the fixture any earlier than this (e.g. right after
    // sending the didOpen above) races creation 1's own process startup:
    // if the repair wins, creation 1 reads the GOOD scenario instead of
    // the crash one, never fails at all, and this test is exercising
    // nothing.
    let failure = proxy
        .wait_for_notification("window/showMessage", 5000)
        .await;
    let failure_msg = failure.params.as_ref().unwrap()["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        failure_msg.contains("Failed to start LSP backend"),
        "expected a backend-start failure message, got: {failure_msg}"
    );

    // Heal the venv for creation 2: exactly one restoration didOpen, for
    // the still-open document only.
    let scenario_retry = serde_json::json!({
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
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "verify hover b" } } }]
            },
            {
                "expect": { "method": "shutdown" },
                "actions": [{ "type": "respond", "body": null }]
            }
        ]
    });
    support::write_venv_fixture(&pkg_dir, &scenario_retry);

    // Generous margin for the queued didClose to actually get replayed
    // (dispatched) — it requires no further I/O (the venv is absent, so
    // the forward after cache removal is a no-op), so this is far more
    // than it needs, but documents the ordering this test depends on: the
    // didClose's cache removal must complete before the didOpen below
    // starts creation 2's restoration snapshot.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Trigger a retry via a normal request: a plain didOpen for a second,
    // still-open document. The venv is absent (creation 1 failed, nothing
    // pooled), so this starts creation 2 and its own restoration snapshot
    // is taken synchronously from this same call — it must contain only
    // this document, not the closed one.
    let open_doc = pkg_dir.join("b.py");
    std::fs::write(&open_doc, "b = 1\n").unwrap();
    let open_doc_uri = support::path_to_uri(&open_doc);
    proxy.did_open(&open_doc_uri, "b = 1\n").await;

    // Only succeeds if creation 2's mock progressed past its single
    // restoration didOpen without a ghost desyncing the scenario.
    let verify = proxy
        .request(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": &open_doc_uri },
                "position": { "line": 0, "character": 0 }
            }),
        )
        .await;
    assert!(
        verify.error.is_none(),
        "verifying hover failed: {:?}",
        verify.error
    );
    assert_eq!(
        verify.result.as_ref().unwrap()["contents"]["value"],
        "verify hover b"
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}

/// E2E (blocking review finding, round 4): a `didClose` that overflows the
/// Creating queue's `MAX_CREATING_QUEUE_LEN` cap must still update the
/// document cache — not just the failure path (round 3) but the capacity
/// path too. `forward_or_queue_for_venv` reports `Dropped` on overflow, and
/// the caller must apply the deferred cache mutation immediately in that
/// case, since a dropped message is never replayed.
///
/// The queue is filled with a second document's own didOpen plus
/// `MAX_CREATING_QUEUE_LEN - 1` `didChange` notifications for it — not the
/// first document's, since that one gets closed by the overflow itself:
/// once its deferred cache removal runs immediately (this round's fix), it
/// stops resolving to any venv at all, and a same-document filler queued
/// behind it would silently no-op on replay (`didChange` for an unopened
/// document) instead of proving anything. Backend 1's scenario absorbs
/// those `MAX_CREATING_QUEUE_LEN` replayed messages (queued while `pkg` was
/// `Creating`, then forwarded live once it's `Ready`) generated
/// programmatically, the same shape as the diagnostics-burst tests above. A
/// `window/logMessage` action on the last one is this test's synchronizing
/// round-trip: it can only arrive after every queued notification has
/// actually been replayed. Backend 2's scenario (after a forced respawn) is
/// strictly ordered with exactly one restoration `didOpen` — for the
/// still-open second document, not the overflow-closed first one. A ghost
/// replay of the closed document desyncs it and crashes the mock, so the
/// verifying hover never returns its scripted value.
#[tokio::test]
async fn didclose_overflow_prevents_ghost_replay_on_respawn_e2e() {
    let mut steps_1 = vec![
        serde_json::json!({
            "expect": { "method": "initialize" },
            "actions": [
                { "type": "sleep_ms", "ms": 300 },
                { "type": "respond", "body": { "capabilities": { "hoverProvider": true } } }
            ]
        }),
        serde_json::json!({ "expect": { "method": "initialized" }, "actions": [] }),
        // Restoration of the closed-to-be document (its creation-time
        // snapshot predates the overflow, so this is expected regardless).
        serde_json::json!({ "expect": { "method": "textDocument/didOpen" }, "actions": [] }),
        // The still-open document's own didOpen, queued (CreatingInFlight)
        // behind the same creation, replayed here.
        serde_json::json!({ "expect": { "method": "textDocument/didOpen" }, "actions": [] }),
    ];
    for i in 0..MAX_CREATING_QUEUE_LEN - 1 {
        let actions = if i == MAX_CREATING_QUEUE_LEN - 2 {
            serde_json::json!([{
                "type": "notify",
                "method": "window/logMessage",
                "params": { "type": 3, "message": "replay drained" }
            }])
        } else {
            serde_json::json!([])
        };
        steps_1.push(serde_json::json!({
            "expect": { "method": "textDocument/didChange" },
            "actions": actions
        }));
    }
    let scenario_1 = serde_json::json!({ "on_startup": [], "steps": steps_1 });

    let config = WorkspaceConfig {
        packages: vec![PackageConfig {
            name: "pkg".to_string(),
            scenario: scenario_1,
            has_venv: true,
        }],
    };

    let (temp_dir, root) = support::setup_test_workspace(&config);
    let pkg_dir = root.join("pkg");
    let mut proxy = ProxyUnderTest::spawn_with_env(
        temp_dir,
        root.clone(),
        &root,
        &[("TYPEMUX_CC_VENV_CHECK_INTERVAL", "1")],
    );

    let root_uri = support::path_to_uri(&root);
    let init_resp = proxy.initialize(&root_uri).await;
    assert!(
        init_resp.error.is_none(),
        "initialize should not return an error"
    );
    proxy.send_initialized().await;

    let closed_doc = pkg_dir.join("a.py");
    std::fs::write(&closed_doc, "a = 1\n").unwrap();
    let closed_doc_uri = support::path_to_uri(&closed_doc);
    // Starts creation 1 (300ms handshake delay before it resolves); this
    // didOpen's own snapshot covers it, so it's not queued.
    proxy.did_open(&closed_doc_uri, "a = 1\n").await;

    let open_doc = pkg_dir.join("b.py");
    std::fs::write(&open_doc, "b = 1\n").unwrap();
    let open_doc_uri = support::path_to_uri(&open_doc);
    // Creation is already in flight: queues (item 1 of the cap).
    proxy.did_open(&open_doc_uri, "b = 1\n").await;

    // Fill the rest of the Creating queue with didChange for the still-open
    // document, all well within the 300ms window.
    for i in 0..MAX_CREATING_QUEUE_LEN - 1 {
        proxy
            .did_change(
                &open_doc_uri,
                i64::try_from(i).unwrap() + 2,
                &format!("b = {}\n", i + 2),
            )
            .await;
    }

    // The queue is now at its cap (1 didOpen + 63 didChange = 64): this
    // didClose for the OTHER document overflows and is dropped (a dedup'd
    // window/showMessage is sent instead of queueing).
    proxy
        .notify(
            "textDocument/didClose",
            serde_json::json!({ "textDocument": { "uri": &closed_doc_uri } }),
        )
        .await;

    // Synchronizing round-trip: this can only arrive once the mock has
    // received (and the proxy has therefore already fully replayed) every
    // one of the queued messages.
    let marker = proxy.wait_for_notification("window/logMessage", 5000).await;
    assert_eq!(marker.params.as_ref().unwrap()["message"], "replay drained");

    // Force a respawn (staleness pattern): replace `.venv`'s identity and
    // push past the 1s debounce.
    std::fs::remove_dir_all(pkg_dir.join(".venv")).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

    // Creation 2: exactly one restoration didOpen, for the still-open
    // document only.
    let scenario_2 = serde_json::json!({
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
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "verify hover b" } } }]
            },
            {
                "expect": { "method": "shutdown" },
                "actions": [{ "type": "respond", "body": null }]
            }
        ]
    });
    support::write_venv_fixture(&pkg_dir, &scenario_2);
    // Different-length pyvenv.cfg content: belt-and-braces against an
    // inode/mtime collision defeating identity-change detection.
    std::fs::write(
        pkg_dir.join(".venv/pyvenv.cfg"),
        "home = /usr/bin\nversion = 3.12\n",
    )
    .unwrap();

    // Trigger the staleness check via a didChange for the still-open
    // document (the closed one is expected to no longer resolve to any
    // venv at all once this round's fix has run its deferred cache
    // removal synchronously on overflow).
    proxy.did_change(&open_doc_uri, 100, "b = 100\n").await;

    let (verify, notifications) = proxy
        .request_collecting(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": &open_doc_uri },
                "position": { "line": 0, "character": 0 }
            }),
        )
        .await;
    assert!(
        verify.error.is_none(),
        "verifying hover failed: {:?}",
        verify.error
    );
    assert_eq!(
        verify.result.as_ref().unwrap()["contents"]["value"],
        "verify hover b"
    );

    let replaced_notice = notifications.iter().any(|n| {
        n.method.as_deref() == Some("window/showMessage")
            && n.params
                .as_ref()
                .and_then(|p| p["message"].as_str())
                .is_some_and(|m| m.contains("replaced"))
    });
    assert!(
        replaced_notice,
        "expected a window/showMessage about the replaced venv, got: {notifications:?}"
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}

/// Text length used to force OS pipe backpressure for the two tests below:
/// comfortably larger than any platform's default pipe buffer (16KB-1MB
/// range), so a restoration `didOpen` carrying this much text can't have its
/// `write_message` complete until the mock has actively drained most of it.
const BACKPRESSURE_TEXT_LEN: usize = 1_000_000;

/// E2E (#134 AC1): a server-initiated request (`workspace/configuration`)
/// emitted during the Creating window — in reaction to the restoration
/// `didOpen`, exactly like real pyright 1.1.407's `workspace/configuration`
/// burst right after `initialized` — is queued (not dropped) and forwarded
/// to the client once creation completes, with a rewritten (proxy-assigned)
/// id. The client's response then routes back to the now-pooled backend
/// through the ordinary `pending_backend_requests` machinery
/// (`client_dispatch::dispatch_client_response`) — proven by a synchronizing
/// `window/logMessage` marker the mock only emits after successfully
/// matching a `"<response>"` step: if the response were lost, malformed, or
/// misrouted, the mock's step sequence would desync and crash instead of
/// reaching the marker, which `wait_for_notification` would then time out
/// waiting for.
///
/// Two large (`BACKPRESSURE_TEXT_LEN`) documents are restored: whichever the
/// mock reads first triggers the request, and while the creation task is
/// then busy writing the SECOND large document (backpressure — see
/// `write_restored_documents`), the venv is provably still `Creating`
/// (`creation_tx.send()` cannot have run yet), so the request is
/// deterministically dispatched via `dispatch_creating_backend_message`, not
/// racing the outcome like a single small document would (verified
/// empirically not to reliably land in this window — the same reason
/// `restoration_diagnostics_reach_client_during_creating_e2e` uses a burst
/// instead of one notification). Both documents are opened before `.venv`
/// exists (`should_restore_document`'s `doc_venv.is_none()` clause) so both
/// land in the SAME creation's restoration snapshot — `open_documents` is a
/// `HashMap`, so which one the creation task writes first isn't otherwise
/// controllable, hence both are large.
#[tokio::test]
// `file_a`/`file_b` (and their `_uri` variants) are deliberately parallel
// names for the two test fixtures this scenario exercises.
#[allow(clippy::similar_names)]
async fn workspace_configuration_during_creating_is_answered_e2e() {
    let scenario = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [{ "type": "respond", "body": { "capabilities": { "hoverProvider": true } } }]
            },
            { "expect": { "method": "initialized" }, "actions": [] },
            {
                "expect": { "method": "textDocument/didOpen" },
                "actions": [
                    { "type": "request", "id": 9, "method": "workspace/configuration", "params": { "items": [{ "section": "python" }] } }
                ]
            },
            { "expect": { "method": "textDocument/didOpen" }, "actions": [] },
            {
                "expect": { "method": "<response>" },
                "actions": [
                    { "type": "notify", "method": "window/logMessage", "params": { "type": 3, "message": "config response received" } }
                ]
            },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover after config" } } }]
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
            scenario: scenario.clone(),
            has_venv: false,
        }],
    };

    let (temp_dir, root) = support::setup_test_workspace(&config);
    let pkg_dir = root.join("pkg");
    let mut proxy = ProxyUnderTest::spawn(temp_dir, root.clone(), &root);

    let root_uri = support::path_to_uri(&root);
    let init_resp = proxy.initialize(&root_uri).await;
    assert!(
        init_resp.error.is_none(),
        "initialize should not return an error"
    );
    proxy.send_initialized().await;

    let large_text = "x".repeat(BACKPRESSURE_TEXT_LEN);

    let file_a = pkg_dir.join("a.py");
    std::fs::write(&file_a, "a = 1\n").unwrap();
    let file_a_uri = support::path_to_uri(&file_a);
    let file_b = pkg_dir.join("b.py");
    std::fs::write(&file_b, "b = 1\n").unwrap();
    let file_b_uri = support::path_to_uri(&file_b);

    // No `.venv` yet: both cache with `venv: None`, neither triggers creation.
    proxy.did_open(&file_a_uri, &large_text).await;
    proxy.did_open(&file_b_uri, &large_text).await;

    // Synchronizing round-trip: `did_open` is a fire-and-forget notification,
    // so awaiting its write only proves the bytes left the harness, not that
    // the proxy has dispatched (parsed, cached) either 1MB message yet. A
    // hover here forces a full round trip through the same FIFO
    // `client_msg_rx`, guaranteeing both prior didOpens are fully processed
    // (and hence cached with `venv: None`) before `.venv` is written below —
    // otherwise `write_venv_fixture` can race ahead of the harness's own
    // writes and get observed by `find_venv` on the SECOND didOpen already,
    // starting creation from the wrong place with a 1-document snapshot.
    let presync = proxy
        .request(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": &file_a_uri },
                "position": { "line": 0, "character": 0 }
            }),
        )
        .await;
    assert!(
        presync.error.is_some(),
        "hover before .venv exists should error (no backend yet)"
    );

    // `.venv` now exists.
    support::write_venv_fixture(&pkg_dir, &scenario);

    // Re-open a.py: `find_venv` now succeeds, starting creation. Its
    // restoration snapshot captures a.py (freshly resolved) AND b.py (still
    // cached with `venv: None`, matched via `should_restore_document`'s
    // second clause) — both large.
    proxy.did_open(&file_a_uri, &large_text).await;

    let config_req = proxy.read_next().await;
    assert_eq!(
        config_req.method.as_deref(),
        Some("workspace/configuration"),
        "expected the backend's workspace/configuration request, got: {config_req:?}"
    );
    assert!(
        matches!(&config_req.id, Some(RpcId::Number(n)) if *n < 0),
        "server-initiated request must arrive with a proxy-assigned (negative) id, got: {:?}",
        config_req.id
    );
    assert_ne!(
        config_req.id,
        Some(RpcId::Number(9)),
        "the backend's own id must never reach the client directly"
    );

    proxy
        .respond_to_backend_request(&config_req, serde_json::json!([{ "tabSize": 4 }]))
        .await;

    // Only arrives if the mock's `"<response>"` step actually matched the
    // routed-back response — proves the backend received it.
    let marker = proxy.wait_for_notification("window/logMessage", 5000).await;
    assert_eq!(
        marker.params.as_ref().unwrap()["message"],
        "config response received"
    );

    let hover = proxy
        .request(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": &file_a_uri },
                "position": { "line": 0, "character": 0 }
            }),
        )
        .await;
    assert!(hover.error.is_none(), "hover failed: {:?}", hover.error);
    assert_eq!(
        hover.result.as_ref().unwrap()["contents"]["value"],
        "hover after config"
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}

/// E2E (#134 AC2): a server-initiated request queued during Creating must
/// not leak into `pending_backend_requests` when that creation FAILS — no
/// response is ever registered/expected for it (registration happens only
/// at forward time, on success), so there is nothing to leak by
/// construction. This proves it observably: no response for the dropped
/// request ever reaches the client, and a subsequent, unrelated request
/// completes normally (no corrupted pending-request state left behind).
///
/// Deterministic failure (unlike `dead_backend_during_creation_does_not_pool_as_zombie_e2e`,
/// which tolerates a race between two failure paths): two large
/// (`BACKPRESSURE_TEXT_LEN`) documents are restored. The mock reacts to the
/// FIRST one it reads with `[request, crash]`; backpressure guarantees it
/// has fully read, reacted, and exited before the creation task's write of
/// the SECOND document is even attempted, so that write hits a closed pipe
/// and fails. `write_restored_documents` returns `Err` directly —
/// `handle_creation_outcome` takes the failure arm without ever calling
/// `child.try_wait()`, so the zombie-insertion race this test's sibling
/// tolerates cannot happen here: the success arm (which would forward
/// `server_requests` to the client) never runs.
#[tokio::test]
// `file_a`/`file_b` (and their `_uri` variants) are deliberately parallel
// names for the two test fixtures this scenario exercises.
#[allow(clippy::similar_names)]
async fn queued_server_request_dropped_on_creation_failure_e2e() {
    let scenario_dying = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [{ "type": "respond", "body": { "capabilities": { "hoverProvider": true } } }]
            },
            { "expect": { "method": "initialized" }, "actions": [] },
            {
                "expect": { "method": "textDocument/didOpen" },
                "actions": [
                    { "type": "request", "id": 1, "method": "workspace/configuration", "params": { "items": [{ "section": "python" }] } },
                    { "type": "sleep_ms", "ms": 200 },
                    { "type": "crash" }
                ]
            }
        ]
    });

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
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover good" } } }]
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
                name: "dying".to_string(),
                scenario: scenario_dying.clone(),
                has_venv: false,
            },
            PackageConfig {
                name: "good".to_string(),
                scenario: scenario_good,
                has_venv: true,
            },
        ],
    };

    let (temp_dir, root) = support::setup_test_workspace(&config);
    let dying_dir = root.join("dying");
    let mut proxy = ProxyUnderTest::spawn(temp_dir, root.clone(), &root);

    let root_uri = support::path_to_uri(&root);
    let init_resp = proxy.initialize(&root_uri).await;
    assert!(
        init_resp.error.is_none(),
        "initialize should not return an error"
    );
    proxy.send_initialized().await;

    let large_text = "x".repeat(BACKPRESSURE_TEXT_LEN);

    let file_a = dying_dir.join("a.py");
    std::fs::write(&file_a, "a = 1\n").unwrap();
    let file_a_uri = support::path_to_uri(&file_a);
    let file_b = dying_dir.join("b.py");
    std::fs::write(&file_b, "b = 1\n").unwrap();
    let file_b_uri = support::path_to_uri(&file_b);

    // No `.venv` yet: both cache with `venv: None`, neither triggers creation.
    proxy.did_open(&file_a_uri, &large_text).await;
    proxy.did_open(&file_b_uri, &large_text).await;

    // Synchronizing round-trip — see the sibling AC1 test's identical step
    // for why this is required before writing `.venv` below (both didOpens
    // must be fully dispatched first, not just written to the harness's own
    // pipe).
    let presync = proxy
        .request(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": &file_a_uri },
                "position": { "line": 0, "character": 0 }
            }),
        )
        .await;
    assert!(
        presync.error.is_some(),
        "hover before .venv exists should error (no backend yet)"
    );

    // `.venv` now exists (the dying scenario).
    support::write_venv_fixture(&dying_dir, &scenario_dying);

    // Re-open a.py: starts creation. Restoration snapshot captures both
    // large documents — see this test's doc comment for why the write of
    // whichever one goes second is guaranteed to fail.
    proxy.did_open(&file_a_uri, &large_text).await;

    // The dying venv's creation failure is reported via a dedup'd
    // window/showMessage (#26/#92 containment) — synchronizing round-trip:
    // by the time this arrives, `handle_creation_outcome` has already run
    // for the dying venv, so any leaked `pending_backend_requests` entry
    // would already exist. `_collecting` (not `wait_for_notification`,
    // which silently discards non-matching messages): the dropped
    // workspace/configuration request, if wrongly forwarded, would likely
    // arrive BEFORE this failure notification — a plain discard-based wait
    // would eat it before the check below ever saw it.
    let (failure, before_failure) = proxy
        .wait_for_notification_collecting("window/showMessage", 5000)
        .await;
    let failure_msg = failure.params.as_ref().unwrap()["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        failure_msg.contains("Failed to start LSP backend"),
        "expected a backend-start failure message, got: {failure_msg}"
    );
    assert!(
        before_failure.is_empty(),
        "no message should have arrived before the failure notification, got: {before_failure:?}"
    );

    // No response for the dropped workspace/configuration request ever
    // reaches the client: give it a bounded window to (wrongly) show up.
    let stray = proxy.drain_notifications(300).await;
    assert!(
        stray.is_empty(),
        "no message should have arrived for the dropped server request, got: {stray:?}"
    );

    // A subsequent, unrelated request (different venv) must complete
    // normally — proves `pending_backend_requests`/`pending_requests`
    // weren't corrupted by the failed creation.
    let file_good = root.join("good/main.py");
    std::fs::write(&file_good, "g = 1\n").unwrap();
    let file_good_uri = support::path_to_uri(&file_good);
    proxy.did_open(&file_good_uri, "g = 1\n").await;

    let hover_good = proxy
        .request(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": &file_good_uri },
                "position": { "line": 0, "character": 0 }
            }),
        )
        .await;
    assert!(
        hover_good.error.is_none(),
        "hover(good) failed: {:?}",
        hover_good.error
    );
    assert_eq!(
        hover_good.result.as_ref().unwrap()["contents"]["value"],
        "hover good"
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}
