mod support;

use std::collections::HashMap;
use support::{PackageConfig, ProxyUnderTest, WorkspaceConfig};
use typemux_cc::message::{RpcId, RpcMessage};

/// E2E fixture (#104): a mock scenario shaped exactly like real pyright
/// 1.1.407's captured startup sequence — a `window/workDoneProgress/create`
/// request with a UUID-shaped string token, an `$/progress` begin with an
/// EMPTY title, a report carrying the human-readable message, then an end.
/// The empty title is deliberate: it's what the real capture showed, and it
/// proves the warmup gate can't be (and isn't) implemented via title
/// matching — only the token identity is structural enough to rely on.
///
/// Re-validates the drain mechanism against this realistic shape rather
/// than a synthetic one-notification token.
#[tokio::test]
async fn pyright_capture_fixture_gates_warmup_on_indexing_token() {
    const TOKEN: &str = "7f0266a6-63a2-43f5-bf5d-3ec636af193b";

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
                    { "type": "request", "id": 3, "method": "window/workDoneProgress/create", "params": { "token": TOKEN } },
                    { "type": "notify", "method": "$/progress", "params": { "token": TOKEN, "value": { "kind": "begin", "title": "" } } },
                    { "type": "notify", "method": "$/progress", "params": { "token": TOKEN, "value": { "kind": "report", "message": "2 files to analyze" } } },
                    { "type": "notify", "method": "$/progress", "params": { "token": TOKEN, "value": { "kind": "end" } } },
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
    let mut proxy = ProxyUnderTest::spawn_with_env(
        temp_dir,
        root.clone(),
        &root,
        &[("TYPEMUX_CC_WARMUP_TIMEOUT", "10")],
    );

    let root_uri = support::path_to_uri(&root);
    let init_resp = proxy.initialize(&root_uri).await;
    assert!(init_resp.error.is_none());
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

    let hover1 = proxy.request("textDocument/hover", doc_params()).await;
    assert!(hover1.error.is_none());

    let def_id = proxy
        .send_request("textDocument/definition", doc_params())
        .await;
    let hover2_id = proxy.send_request("textDocument/hover", doc_params()).await;

    // Filtered to `is_response()`: the mock's `window/workDoneProgress/create`
    // request is also forwarded to the client in this window and would
    // otherwise be miscounted as one of the two responses awaited here.
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
    assert!(hover2.error.is_none());
    assert_eq!(
        hover2.result.as_ref().unwrap()["contents"]["value"],
        "hover triggering ready"
    );

    let def_resp = &responses[&def_id];
    assert!(
        def_resp.error.is_none(),
        "queued definition should be drained once the indexing token's end arrives, got: {:?}",
        def_resp.error
    );
    assert_eq!(def_resp.result.as_ref().unwrap()["uri"], "file:///pkg/a.py");

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(shutdown_resp.error.is_none());
}

/// E2E (AC2, #104): two pooled backends whose `window/workDoneProgress/create`
/// and `$/progress` all use the IDENTICAL raw token value must not collide
/// client-side — each backend's stream must arrive under its own distinct,
/// proxy-namespaced token, and neither stream's begin/report/end intermixes
/// with the other's.
///
/// Detection power: without namespacing, `tokens_a[0] == tokens_b[0] ==
/// "shared-token"` (the raw value both backends emit), directly failing the
/// `assert_ne!` below.
#[tokio::test]
// `file_a`/`file_b` (and their `_uri` variants) are deliberately parallel
// names for the two test fixtures this scenario exercises — see the
// identical pattern (and rationale) in `tests/multi_venv_test.rs`.
#[allow(clippy::similar_names)]
async fn identical_raw_tokens_across_backends_produce_distinct_client_tokens() {
    const RAW_TOKEN: &str = "shared-token";

    let progress_actions = |suffix: &str| {
        serde_json::json!([
            { "type": "request", "id": 1, "method": "window/workDoneProgress/create", "params": { "token": RAW_TOKEN } },
            { "type": "notify", "method": "$/progress", "params": { "token": RAW_TOKEN, "value": { "kind": "begin", "title": "" } } },
            { "type": "notify", "method": "$/progress", "params": { "token": RAW_TOKEN, "value": { "kind": "report", "message": "indexing" } } },
            { "type": "notify", "method": "$/progress", "params": { "token": RAW_TOKEN, "value": { "kind": "end" } } },
            { "type": "respond", "body": { "contents": { "kind": "plaintext", "value": format!("hover-{suffix}") } } }
        ])
    };

    let scenario_for = |suffix: &str| {
        serde_json::json!({
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
                    "actions": progress_actions(suffix)
                },
                {
                    "expect": { "method": "shutdown" },
                    "actions": [{ "type": "respond", "body": null }]
                }
            ]
        })
    };

    let config = WorkspaceConfig {
        packages: vec![
            PackageConfig {
                name: "proj-a".to_string(),
                scenario: scenario_for("a"),
                has_venv: true,
            },
            PackageConfig {
                name: "proj-b".to_string(),
                scenario: scenario_for("b"),
                has_venv: true,
            },
        ],
    };

    let (temp_dir, root) = support::setup_test_workspace(&config);
    let mut proxy = ProxyUnderTest::spawn(temp_dir, root.clone(), &root);

    let root_uri = support::path_to_uri(&root);
    let init_resp = proxy.initialize(&root_uri).await;
    assert!(init_resp.error.is_none());
    proxy.send_initialized().await;

    let file_a = root.join("proj-a/main.py");
    std::fs::write(&file_a, "a = 1\n").unwrap();
    let file_a_uri = support::path_to_uri(&file_a);
    proxy.did_open(&file_a_uri, "a = 1\n").await;

    let (hover_a, extra_a) = proxy
        .request_collecting(
            "textDocument/hover",
            serde_json::json!({ "textDocument": { "uri": &file_a_uri }, "position": { "line": 0, "character": 0 } }),
        )
        .await;
    assert!(hover_a.error.is_none());
    assert_eq!(
        hover_a.result.as_ref().unwrap()["contents"]["value"],
        "hover-a"
    );

    let file_b = root.join("proj-b/main.py");
    std::fs::write(&file_b, "b = 2\n").unwrap();
    let file_b_uri = support::path_to_uri(&file_b);
    proxy.did_open(&file_b_uri, "b = 2\n").await;

    let (hover_b, extra_b) = proxy
        .request_collecting(
            "textDocument/hover",
            serde_json::json!({ "textDocument": { "uri": &file_b_uri }, "position": { "line": 0, "character": 0 } }),
        )
        .await;
    assert!(hover_b.error.is_none());
    assert_eq!(
        hover_b.result.as_ref().unwrap()["contents"]["value"],
        "hover-b"
    );

    let progress_tokens = |msgs: &[RpcMessage]| -> Vec<String> {
        msgs.iter()
            .filter(|m| {
                matches!(
                    m.method.as_deref(),
                    Some("$/progress" | "window/workDoneProgress/create")
                )
            })
            .map(|m| {
                m.params.as_ref().unwrap()["token"]
                    .as_str()
                    .expect("token must be a string once namespaced")
                    .to_string()
            })
            .collect()
    };

    let tokens_a = progress_tokens(&extra_a);
    let tokens_b = progress_tokens(&extra_b);

    assert_eq!(
        tokens_a.len(),
        4,
        "expected create + begin + report + end for backend A, got {tokens_a:?}"
    );
    assert_eq!(
        tokens_b.len(),
        4,
        "expected create + begin + report + end for backend B, got {tokens_b:?}"
    );

    // Each backend's own stream must be internally consistent (one token
    // throughout — not intermixed with the other backend's).
    assert!(
        tokens_a.iter().all(|t| *t == tokens_a[0]),
        "backend A's create/begin/report/end must all share one client-visible token, got {tokens_a:?}"
    );
    assert!(
        tokens_b.iter().all(|t| *t == tokens_b[0]),
        "backend B's create/begin/report/end must all share one client-visible token, got {tokens_b:?}"
    );

    // The actual #104 collision: both backends emit the identical RAW
    // token, but the client must see two DIFFERENT tokens.
    assert_ne!(
        tokens_a[0], tokens_b[0],
        "two backends sharing the same raw token must not collide client-side"
    );
    // And namespacing must have actually happened (not a passthrough that
    // coincidentally differs).
    assert_ne!(tokens_a[0], RAW_TOKEN);
    assert_ne!(tokens_b[0], RAW_TOKEN);

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(shutdown_resp.error.is_none());
}

/// E2E (AC3, #104): a warming backend must NOT be marked Ready by a
/// `$/progress end` that doesn't belong to its recorded indexing token, but
/// MUST be marked Ready once that token's own end arrives.
///
/// Detection power: each hover in the sequence is awaited (not just sent)
/// before the next is issued, so the proxy has fully finished reacting to
/// the unrelated end — including any (buggy) premature queue drain — before
/// the third hover is even sent. If `textDocument/definition` were
/// prematurely forwarded, it would already be sitting in the backend's
/// stdin pipe ahead of the third hover, so the mock's own step sequencing
/// (expecting `textDocument/hover` next, not `textDocument/definition`)
/// crashes it with a loud, deterministic mismatch — no sleep-based race.
#[tokio::test]
async fn unrelated_progress_end_does_not_ready_backend_but_matching_token_does() {
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
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover1" } } }]
            },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [
                    { "type": "notify", "method": "$/progress", "params": { "token": "unrelated", "value": { "kind": "end" } } },
                    { "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover2" } } }
                ]
            },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [
                    { "type": "request", "id": 5, "method": "window/workDoneProgress/create", "params": { "token": "real-token" } },
                    { "type": "notify", "method": "$/progress", "params": { "token": "real-token", "value": { "kind": "end" } } },
                    { "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover3" } } }
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
    let mut proxy = ProxyUnderTest::spawn_with_env(
        temp_dir,
        root.clone(),
        &root,
        &[("TYPEMUX_CC_WARMUP_TIMEOUT", "10")],
    );

    let root_uri = support::path_to_uri(&root);
    let init_resp = proxy.initialize(&root_uri).await;
    assert!(init_resp.error.is_none());
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

    // Sync #1: spawns the backend (Warming) and confirms it's Ready-in-pool.
    let hover1 = proxy.request("textDocument/hover", doc_params()).await;
    assert!(hover1.error.is_none());
    assert_eq!(
        hover1.result.as_ref().unwrap()["contents"]["value"],
        "hover1"
    );

    // Queued: the backend is still Warming (no indexing token recorded yet).
    let def_id = proxy
        .send_request("textDocument/definition", doc_params())
        .await;

    // Sync #2 (BLOCKING wait, not send_request): by the time this returns,
    // the proxy has fully processed the unrelated end — including any
    // (buggy) premature drain — since the `$/progress` notification and
    // this hover's response arrive from the backend strictly in that order
    // over the same channel.
    let hover2 = proxy.request("textDocument/hover", doc_params()).await;
    assert!(hover2.error.is_none());
    assert_eq!(
        hover2.result.as_ref().unwrap()["contents"]["value"],
        "hover2"
    );

    // Sync #3: carries the REAL indexing token's end. Only sent after #2
    // fully resolved, so the mock's step order (hover3, not the queued
    // definition) is deterministic proof the unrelated end above did not
    // drain the queue.
    let hover3 = proxy.request("textDocument/hover", doc_params()).await;
    assert!(hover3.error.is_none());
    assert_eq!(
        hover3.result.as_ref().unwrap()["contents"]["value"],
        "hover3"
    );

    let def_resp = proxy.wait_for_response(def_id, 5_000).await;
    assert!(
        def_resp.error.is_none(),
        "definition should drain once the matching indexing token's end arrives, got: {:?}",
        def_resp.error
    );
    assert_eq!(def_resp.result.as_ref().unwrap()["uri"], "file:///pkg/a.py");

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(shutdown_resp.error.is_none());
}

/// E2E (AC4, #104): `window/workDoneProgress/cancel` carrying a
/// proxy-namespaced token routes to the ONE backend that owns it, with the
/// ORIGINAL (un-prefixed) token restored — never broadcast to every backend
/// with the still-prefixed token, which the generic
/// `dispatch_client_notification` fallback would otherwise do (it has no
/// `textDocument.uri` to route by).
///
/// Detection power, both directions:
/// - Backend A's scenario asserts (via the mock DSL's `expect.token`) that
///   it receives exactly `"original-a-token"` — the raw value it minted,
///   not the namespaced one the client sent back. A subsequent hover on A
///   only succeeds if A's mock actually matched that step and progressed
///   (if it crashed on a token mismatch, the hover call panics/times out).
/// - Backend B's scenario has NO step expecting
///   `window/workDoneProgress/cancel` between its two hovers; if the cancel
///   were wrongly broadcast to B, its next read there would mismatch and
///   crash the mock, and the second hover on B would fail identically.
#[tokio::test]
// `file_a`/`file_b` (and their `_uri` variants) are deliberately parallel
// names for the two test fixtures this scenario exercises — see the
// identical pattern (and rationale) in `tests/multi_venv_test.rs`.
#[allow(clippy::similar_names)]
async fn workdone_progress_cancel_routes_to_owning_backend_with_original_token() {
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
                "actions": [
                    { "type": "request", "id": 1, "method": "window/workDoneProgress/create", "params": { "token": "original-a-token" } },
                    { "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover-a" } } }
                ]
            },
            {
                "expect": { "method": "window/workDoneProgress/cancel", "token": "original-a-token" },
                "actions": []
            },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover-a-after-cancel" } } }]
            },
            {
                "expect": { "method": "shutdown" },
                "actions": [{ "type": "respond", "body": null }]
            }
        ]
    });

    let scenario_b = serde_json::json!({
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
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover-b" } } }]
            },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover-b-after-cancel" } } }]
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
                name: "proj-a".to_string(),
                scenario: scenario_a,
                has_venv: true,
            },
            PackageConfig {
                name: "proj-b".to_string(),
                scenario: scenario_b,
                has_venv: true,
            },
        ],
    };

    let (temp_dir, root) = support::setup_test_workspace(&config);
    let mut proxy = ProxyUnderTest::spawn(temp_dir, root.clone(), &root);

    let root_uri = support::path_to_uri(&root);
    let init_resp = proxy.initialize(&root_uri).await;
    assert!(init_resp.error.is_none());
    proxy.send_initialized().await;

    let file_a = root.join("proj-a/main.py");
    std::fs::write(&file_a, "a = 1\n").unwrap();
    let file_a_uri = support::path_to_uri(&file_a);
    proxy.did_open(&file_a_uri, "a = 1\n").await;

    let hover_params = |uri: &str| serde_json::json!({ "textDocument": { "uri": uri }, "position": { "line": 0, "character": 0 } });

    let (hover_a, extra_a) = proxy
        .request_collecting("textDocument/hover", hover_params(&file_a_uri))
        .await;
    assert!(hover_a.error.is_none());
    assert_eq!(
        hover_a.result.as_ref().unwrap()["contents"]["value"],
        "hover-a"
    );

    let create_msg = extra_a
        .iter()
        .find(|m| m.method.as_deref() == Some("window/workDoneProgress/create"))
        .expect("backend A's create request should have been forwarded to the client");
    let namespaced_token = create_msg.params.as_ref().unwrap()["token"]
        .as_str()
        .expect("namespaced token must be a string")
        .to_string();
    assert_ne!(
        namespaced_token, "original-a-token",
        "token must be namespaced, not passed through raw"
    );

    let file_b = root.join("proj-b/main.py");
    std::fs::write(&file_b, "b = 2\n").unwrap();
    let file_b_uri = support::path_to_uri(&file_b);
    proxy.did_open(&file_b_uri, "b = 2\n").await;

    let hover_b = proxy
        .request("textDocument/hover", hover_params(&file_b_uri))
        .await;
    assert!(hover_b.error.is_none());
    assert_eq!(
        hover_b.result.as_ref().unwrap()["contents"]["value"],
        "hover-b"
    );

    // Cancel using the namespaced token observed from A's create request.
    proxy
        .notify(
            "window/workDoneProgress/cancel",
            serde_json::json!({ "token": namespaced_token }),
        )
        .await;

    // Proves A received the cancel with the ORIGINAL token restored (its
    // mock scenario would have crashed on a mismatch otherwise, and this
    // call would panic/time out instead of returning normally).
    let post_cancel_a = proxy
        .request("textDocument/hover", hover_params(&file_a_uri))
        .await;
    assert!(post_cancel_a.error.is_none());
    assert_eq!(
        post_cancel_a.result.as_ref().unwrap()["contents"]["value"],
        "hover-a-after-cancel"
    );

    // Proves B did NOT receive the cancel (no broadcast): its mock has no
    // step for it between the two hovers, so a broadcast would have
    // crashed B before this call could succeed.
    let post_cancel_b = proxy
        .request("textDocument/hover", hover_params(&file_b_uri))
        .await;
    assert!(post_cancel_b.error.is_none());
    assert_eq!(
        post_cancel_b.result.as_ref().unwrap()["contents"]["value"],
        "hover-b-after-cancel"
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(shutdown_resp.error.is_none());
}
