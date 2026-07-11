mod support;

use support::{PackageConfig, ProxyUnderTest, WorkspaceConfig};

/// E2E: `workspace/symbol` with two pooled backends fans out to both and
/// merges their result sets into a single response.
///
/// Each scenario's synchronizing `documentSymbol` step (answered before
/// `workspace/symbol`) forces its backend to `Ready` deterministically:
/// `workspace/symbol` fan-out only targets `pool.backends_keys()` (Ready
/// backends), by design (see ARCHITECTURE.md's Creating-state section) — a
/// backend still `Creating` when the fan-out fires is silently excluded, so
/// without this synchronization the test would race backend creation.
#[tokio::test]
// `file_a`/`file_b` (and their `_uri`/`sync_a`/`sync_b` variants) are
// deliberately parallel names for the two test fixtures this scenario
// exercises.
#[allow(clippy::similar_names)]
async fn fanout_merges_results_from_two_backends() {
    let scenario_a = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [{ "type": "respond", "body": { "capabilities": { "workspaceSymbolProvider": true } } }]
            },
            { "expect": { "method": "initialized" }, "actions": [] },
            { "expect": { "method": "textDocument/didOpen" }, "actions": [] },
            {
                "expect": { "method": "textDocument/documentSymbol" },
                "actions": [{ "type": "respond", "body": [] }]
            },
            {
                "expect": { "method": "workspace/symbol" },
                "actions": [{ "type": "respond", "body": [
                    {
                        "name": "FooA",
                        "kind": 12,
                        "location": {
                            "uri": "file:///proj-a/main.py",
                            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 3 } }
                        }
                    }
                ] }]
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
                "actions": [{ "type": "respond", "body": { "capabilities": { "workspaceSymbolProvider": true } } }]
            },
            { "expect": { "method": "initialized" }, "actions": [] },
            { "expect": { "method": "textDocument/didOpen" }, "actions": [] },
            {
                "expect": { "method": "textDocument/documentSymbol" },
                "actions": [{ "type": "respond", "body": [] }]
            },
            {
                "expect": { "method": "workspace/symbol" },
                "actions": [{ "type": "respond", "body": [
                    {
                        "name": "FooB",
                        "kind": 12,
                        "location": {
                            "uri": "file:///proj-b/main.py",
                            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 3 } }
                        }
                    }
                ] }]
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
    // Start proxy from workspace root (no fallback venv at root level), so
    // each backend is spawned via the pool-miss path (one "initialized" each).
    let mut proxy = ProxyUnderTest::spawn(temp_dir, root.clone(), &root);

    let root_uri = support::path_to_uri(&root);
    let init_resp = proxy.initialize(&root_uri).await;
    assert!(
        init_resp.error.is_none(),
        "initialize should not return an error"
    );
    proxy.send_initialized().await;

    // didOpen on each package starts its backend's creation.
    let file_a = root.join("proj-a/main.py");
    std::fs::write(&file_a, "a = 1\n").unwrap();
    let file_a_uri = support::path_to_uri(&file_a);
    proxy.did_open(&file_a_uri, "a = 1\n").await;

    let file_b = root.join("proj-b/main.py");
    std::fs::write(&file_b, "b = 2\n").unwrap();
    let file_b_uri = support::path_to_uri(&file_b);
    proxy.did_open(&file_b_uri, "b = 2\n").await;

    // Synchronizing round-trips: force both backends to `Ready` before the
    // fan-out below (see the test's doc comment).
    let sync_a = proxy
        .request(
            "textDocument/documentSymbol",
            serde_json::json!({ "textDocument": { "uri": &file_a_uri } }),
        )
        .await;
    assert!(
        sync_a.error.is_none(),
        "sync documentSymbol(a) failed: {:?}",
        sync_a.error
    );
    let sync_b = proxy
        .request(
            "textDocument/documentSymbol",
            serde_json::json!({ "textDocument": { "uri": &file_b_uri } }),
        )
        .await;
    assert!(
        sync_b.error.is_none(),
        "sync documentSymbol(b) failed: {:?}",
        sync_b.error
    );

    // workspace/symbol has no document URI: with 2 pooled backends this
    // fans out to both and merges their results.
    let symbol_resp = proxy
        .request("workspace/symbol", serde_json::json!({ "query": "Foo" }))
        .await;
    assert!(
        symbol_resp.error.is_none(),
        "workspace/symbol should not return an error, got: {:?}",
        symbol_resp.error
    );

    let results = symbol_resp
        .result
        .as_ref()
        .and_then(serde_json::Value::as_array)
        .expect("workspace/symbol result should be an array");
    let names: Vec<&str> = results
        .iter()
        .map(|item| item["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        results.len(),
        2,
        "expected merged results from both backends, got: {names:?}"
    );
    assert!(names.contains(&"FooA"), "missing FooA, got: {names:?}");
    assert!(names.contains(&"FooB"), "missing FooB, got: {names:?}");

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}

/// E2E: `workspace/symbol` fanned out to two backends, one of which never
/// responds, returns the other backend's results (not an error, not a
/// hang) once the fan-out timeout elapses.
///
/// Each scenario's synchronizing `documentSymbol` step forces its backend to
/// `Ready` before the fan-out fires — see
/// `fanout_merges_results_from_two_backends`'s doc comment for why.
#[tokio::test]
// `file_a`/`file_b` (and their `_uri`/`sync_a`/`sync_b` variants) are
// deliberately parallel names for the two test fixtures this scenario
// exercises.
#[allow(clippy::similar_names)]
async fn fanout_returns_partial_results_on_backend_timeout() {
    let scenario_a = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [{ "type": "respond", "body": { "capabilities": { "workspaceSymbolProvider": true } } }]
            },
            { "expect": { "method": "initialized" }, "actions": [] },
            { "expect": { "method": "textDocument/didOpen" }, "actions": [] },
            {
                "expect": { "method": "textDocument/documentSymbol" },
                "actions": [{ "type": "respond", "body": [] }]
            },
            {
                "expect": { "method": "workspace/symbol" },
                "actions": [{ "type": "respond", "body": [
                    {
                        "name": "FooA",
                        "kind": 12,
                        "location": {
                            "uri": "file:///proj-a/main.py",
                            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 3 } }
                        }
                    }
                ] }]
            },
            {
                "expect": { "method": "shutdown" },
                "actions": [{ "type": "respond", "body": null }]
            }
        ]
    });

    // Backend B never responds to workspace/symbol: it just sits there.
    // The proxy sends it a best-effort $/cancelRequest once the fan-out
    // times out, so the scenario accounts for that before shutdown.
    let scenario_b = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [{ "type": "respond", "body": { "capabilities": { "workspaceSymbolProvider": true } } }]
            },
            { "expect": { "method": "initialized" }, "actions": [] },
            { "expect": { "method": "textDocument/didOpen" }, "actions": [] },
            {
                "expect": { "method": "textDocument/documentSymbol" },
                "actions": [{ "type": "respond", "body": [] }]
            },
            { "expect": { "method": "workspace/symbol" }, "actions": [] },
            { "expect": { "method": "$/cancelRequest" }, "actions": [] },
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
    // Short fan-out timeout so the test doesn't wait the 5s default.
    let mut proxy = ProxyUnderTest::spawn_with_env(
        temp_dir,
        root.clone(),
        &root,
        &[("TYPEMUX_CC_FANOUT_TIMEOUT", "1")],
    );

    let root_uri = support::path_to_uri(&root);
    let init_resp = proxy.initialize(&root_uri).await;
    assert!(
        init_resp.error.is_none(),
        "initialize should not return an error"
    );
    proxy.send_initialized().await;

    let file_a = root.join("proj-a/main.py");
    std::fs::write(&file_a, "a = 1\n").unwrap();
    let file_a_uri = support::path_to_uri(&file_a);
    proxy.did_open(&file_a_uri, "a = 1\n").await;

    let file_b = root.join("proj-b/main.py");
    std::fs::write(&file_b, "b = 2\n").unwrap();
    let file_b_uri = support::path_to_uri(&file_b);
    proxy.did_open(&file_b_uri, "b = 2\n").await;

    // Synchronizing round-trips: force both backends to `Ready` before the
    // fan-out below (see the test's doc comment).
    let sync_a = proxy
        .request(
            "textDocument/documentSymbol",
            serde_json::json!({ "textDocument": { "uri": &file_a_uri } }),
        )
        .await;
    assert!(
        sync_a.error.is_none(),
        "sync documentSymbol(a) failed: {:?}",
        sync_a.error
    );
    let sync_b = proxy
        .request(
            "textDocument/documentSymbol",
            serde_json::json!({ "textDocument": { "uri": &file_b_uri } }),
        )
        .await;
    assert!(
        sync_b.error.is_none(),
        "sync documentSymbol(b) failed: {:?}",
        sync_b.error
    );

    // Backend B never answers, so the response only arrives once the 1s
    // fan-out timeout fires and the proxy returns backend A's partial
    // results. The harness's 5s read timeout gives ample margin over that.
    let (symbol_resp, notifications) = proxy
        .request_collecting("workspace/symbol", serde_json::json!({ "query": "Foo" }))
        .await;

    assert!(
        symbol_resp.error.is_none(),
        "workspace/symbol should not return an error on partial timeout, got: {:?}",
        symbol_resp.error
    );
    let results = symbol_resp
        .result
        .as_ref()
        .and_then(serde_json::Value::as_array)
        .expect("workspace/symbol result should be an array");
    assert_eq!(
        results.len(),
        1,
        "expected only backend A's result, got: {results:?}"
    );
    assert_eq!(results[0]["name"], "FooA");

    let timeout_warning = notifications.iter().any(|n| {
        n.method.as_deref() == Some("window/showMessage")
            && n.params
                .as_ref()
                .and_then(|p| p["message"].as_str())
                .is_some_and(|m| m.contains("fan-out timeout"))
    });
    assert!(
        timeout_warning,
        "expected a window/showMessage about the fan-out timeout, got: {notifications:?}"
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}
