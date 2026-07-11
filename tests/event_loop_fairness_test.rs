mod support;

use support::{PackageConfig, ProxyUnderTest, WorkspaceConfig};
use typemux_cc::message::RpcId;

/// Number of marker requests answered while the flood is running. With a
/// reintroduced `biased;` client-first (see the detection-power check
/// this test was verified against), the flood starves the backend arm
/// completely — 0 of any number of markers arrive before the deadline —
/// so this isn't tuned for a narrow timing margin; it's just a broad
/// enough sample that "all of them, well within a generous deadline"
/// is an unambiguous pass/fail signal.
const MARKER_COUNT: usize = 150;

/// E2E (blocking review finding): under a continuous flood of client input,
/// backend-originated responses must still arrive within a bounded time —
/// the main select! loop must stay fair among the client, backend,
/// creation-completion, timer, and replay arms, never starving the
/// non-client ones.
///
/// A finite pre-written batch of flood messages is NOT sufficient to catch
/// a `biased;`-with-client-first regression: stdin eventually empties, at
/// which point the client arm stops being immediately ready and the old
/// code's starvation resolves on its own — the test would pass either way,
/// fixed or broken. Feeding messages CONTINUOUSLY from a separately spawned
/// task (`spawn_notification_flood`) keeps stdin non-empty for the whole
/// assertion window, so starvation (if present) can't resolve itself
/// before the timeout fires.
#[tokio::test]
async fn client_flood_does_not_starve_backend_traffic_e2e() {
    let mut steps = vec![
        serde_json::json!({
            "expect": { "method": "initialize" },
            "actions": [{ "type": "respond", "body": { "capabilities": { "hoverProvider": true } } }]
        }),
        serde_json::json!({ "expect": { "method": "initialized" }, "actions": [] }),
        serde_json::json!({ "expect": { "method": "textDocument/didOpen" }, "actions": [] }),
        serde_json::json!({
            "expect": { "method": "textDocument/hover" },
            "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "sync hover" } } }]
        }),
    ];
    for i in 0..MARKER_COUNT {
        steps.push(serde_json::json!({
            "expect": { "method": "textDocument/hover" },
            "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": format!("marker {i}") } } }]
        }));
    }
    let scenario = serde_json::json!({ "on_startup": [], "steps": steps });

    let config = WorkspaceConfig {
        packages: vec![PackageConfig {
            name: "pkg".to_string(),
            scenario,
            has_venv: true,
        }],
    };

    let (temp_dir, root) = support::setup_test_workspace(&config);
    let mut proxy = ProxyUnderTest::spawn_with_env(
        temp_dir,
        root.clone(),
        &root,
        &[("RUST_LOG", "typemux_cc=warn")],
    );
    proxy.spawn_stderr_drain();

    let root_uri = support::path_to_uri(&root);
    let init_resp = proxy.initialize(&root_uri).await;
    assert!(
        init_resp.error.is_none(),
        "initialize should not return an error"
    );
    proxy.send_initialized().await;

    let file_uri = support::path_to_uri(&root.join("pkg/main.py"));
    proxy.did_open(&file_uri, "a = 1\n").await;

    let hover_params = serde_json::json!({
        "textDocument": { "uri": &file_uri },
        "position": { "line": 0, "character": 0 }
    });

    // Synchronizing round-trip: the backend is fully Ready before the flood
    // starts (the markers below should measure event-loop fairness, not
    // backend-creation latency).
    let sync_resp = proxy
        .request("textDocument/hover", hover_params.clone())
        .await;
    assert!(
        sync_resp.error.is_none(),
        "sync hover failed: {:?}",
        sync_resp.error
    );

    // Markers: all sent before the flood starts (spawn_notification_flood
    // takes over the writer after this), answered by the backend WHILE the
    // flood is running. FIFO-scripted, so responses arrive in request order.
    let mut marker_ids = Vec::with_capacity(MARKER_COUNT);
    for _ in 0..MARKER_COUNT {
        marker_ids.push(
            proxy
                .send_request("textDocument/hover", hover_params.clone())
                .await,
        );
    }

    // Flood: a never-opened URI's didSave is dropped by the proxy with a
    // warn (zero backend traffic), so it saturates client input without
    // otherwise perturbing the scenario.
    let flood_uri = support::path_to_uri(&root.join("pkg/never_opened.py"));
    let flood_params = serde_json::json!({ "textDocument": { "uri": flood_uri } });
    let flood_handle = proxy.spawn_notification_flood("textDocument/didSave", flood_params);

    // Generous margin over normal (same-host) drain time for this many
    // responses under fair scheduling (measured well under 100ms with the
    // fix in place), but the flood (only stopped after this returns, never
    // on its own) is still very much in progress for the entire wait — a
    // timeout here means the markers were starved, not merely slow.
    let deadline_ms = 3000;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(deadline_ms);
    let mut responses = std::collections::HashMap::new();
    while responses.len() < MARKER_COUNT {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out after {deadline_ms}ms: only {}/{MARKER_COUNT} marker responses arrived — \
             backend traffic was starved by the client flood",
            responses.len()
        );
        let msg = tokio::time::timeout(remaining, proxy.read_message_raw())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "timed out after {deadline_ms}ms: only {}/{MARKER_COUNT} marker responses arrived — \
                     backend traffic was starved by the client flood",
                    responses.len()
                )
            });
        if let Some(RpcId::Number(id)) = &msg.id {
            if marker_ids.contains(id) {
                responses.insert(*id, msg);
            }
        }
    }
    flood_handle.abort();

    for (i, id) in marker_ids.iter().enumerate() {
        let resp = responses
            .get(id)
            .unwrap_or_else(|| panic!("missing response for marker {i}"));
        assert!(resp.error.is_none(), "marker {i} failed: {:?}", resp.error);
        assert_eq!(
            resp.result.as_ref().unwrap()["contents"]["value"],
            format!("marker {i}"),
            "marker {i} got the wrong response — order corrupted"
        );
    }

    // No graceful shutdown: the flood task owned the writer and was
    // aborted (not returned) rather than drained to completion — Drop
    // kills the child process.
}
