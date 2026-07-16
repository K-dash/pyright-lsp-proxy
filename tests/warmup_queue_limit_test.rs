mod support;

use support::{PackageConfig, ProxyUnderTest, WorkspaceConfig};
use typemux_cc::message::RpcId;

/// Mirrors `MAX_WARMUP_QUEUE_LEN` in `src/backend_pool.rs`. Not exported from
/// the lib crate (the E2E harness only links `framing`/`message` and drives
/// the proxy as a black-box subprocess), so it is duplicated here.
const MAX_WARMUP_QUEUE_LEN: usize = 64;

/// E2E: once `MAX_WARMUP_QUEUE_LEN` index-dependent requests are queued
/// during warmup, the next one is rejected immediately with a JSON-RPC error
/// instead of being queued.
///
/// The backend never sends a `$/progress` end event and the warmup timeout
/// is set far beyond the test's runtime, so the backend stays `Warming` for
/// the whole test — the queue can only shrink via the capacity rejection
/// under test, never via drain.
///
/// A `didOpen` + hover round trip runs first to synchronize backend
/// creation (#140: `initialize` no longer spawns anything by itself). Without
/// it, the flood below would hit an empty pool: the first `definition`
/// request would start Creating and the rest would land in the *Creating*
/// queue (`MAX_CREATING_QUEUE_LEN`, a different branch with the same numeric
/// limit), never exercising the warmup-queue rejection this test targets.
#[tokio::test]
async fn warmup_queue_rejects_once_full() {
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
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover ok" } } }]
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
    // Start proxy from pkg/ (has .venv, detected but not spawned yet — #140)
    // with a warmup timeout far longer than the test, so fail-open never
    // kicks in. RUST_LOG is pinned so a developer's
    // `~/.config/typemux-cc/config` (which may set `RUST_LOG=typemux_cc=trace`)
    // can't reintroduce enough per-request logging overhead to blow the
    // harness's 5s read timeout across MAX_WARMUP_QUEUE_LEN back-to-back
    // requests.
    let mut proxy = ProxyUnderTest::spawn_with_env(
        temp_dir,
        root.clone(),
        &root.join("pkg"),
        &[
            ("TYPEMUX_CC_WARMUP_TIMEOUT", "600"),
            ("RUST_LOG", "typemux_cc=warn"),
        ],
    );

    let root_uri = support::path_to_uri(&root.join("pkg"));
    let init_resp = proxy.initialize(&root_uri).await;
    assert!(
        init_resp.error.is_none(),
        "initialize should not return an error"
    );
    proxy.send_initialized().await;

    let file_a = root.join("pkg/a.py");
    std::fs::write(&file_a, "a = 1\n").unwrap();
    let file_uri = support::path_to_uri(&file_a);
    proxy.did_open(&file_uri, "a = 1\n").await;

    // Synchronizing round trip: creates the backend (Creating → Warming)
    // before the flood below, so the flood exercises the warmup queue
    // instead of the Creating queue.
    let hover = proxy
        .request(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": &file_uri },
                "position": { "line": 0, "character": 0 }
            }),
        )
        .await;
    assert!(
        hover.error.is_none(),
        "synchronizing hover should not return an error, got: {:?}",
        hover.error
    );

    let definition_params = || {
        serde_json::json!({
            "textDocument": { "uri": &file_uri },
            "position": { "line": 0, "character": 0 }
        })
    };

    // Fill the warmup queue to capacity. None of these get a response: the
    // backend never leaves `Warming`, so nothing is ever drained.
    for _ in 0..MAX_WARMUP_QUEUE_LEN {
        proxy
            .send_request("textDocument/definition", definition_params())
            .await;
    }

    // The next request overflows the queue and must be rejected immediately.
    let overflow_id = proxy
        .send_request("textDocument/definition", definition_params())
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
