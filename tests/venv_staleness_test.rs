mod support;

use support::{PackageConfig, ProxyUnderTest, WorkspaceConfig};

/// E2E: venv identity tracking — a replaced `.venv` (e.g. `uv sync`) is
/// detected on the next debounced check and the backend is respawned with
/// the new environment, exactly once.
///
/// These are the suite's first timing-based tests. `TYPEMUX_CC_VENV_CHECK_INTERVAL=1`
/// keeps them short; sleeps carry generous margins over the 1s debounce and
/// the 2s (= 2 × interval) missing-file grace to absorb CI jitter.
#[tokio::test]
async fn replaced_venv_triggers_single_backend_respawn() {
    // First lifetime: hover works against the original venv.
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

    let hover_params = serde_json::json!({
        "textDocument": { "uri": &file_a_uri },
        "position": { "line": 0, "character": 0 }
    });

    // First hover lands within the debounce window: no staleness check yet.
    let hover1 = proxy
        .request("textDocument/hover", hover_params.clone())
        .await;
    assert!(
        hover1.error.is_none(),
        "hover against original venv should succeed, got error: {:?}",
        hover1.error
    );
    assert_eq!(
        hover1.result.as_ref().unwrap()["contents"]["value"],
        "hover life1"
    );

    // Simulate `uv sync`: delete and recreate `.venv`. The sleep separates
    // the two pyvenv.cfg files in mtime and defeats inode-reuse collisions,
    // and also pushes past the 1s debounce so the next hover checks.
    std::fs::remove_dir_all(pkg_dir.join(".venv")).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

    // Second lifetime: the respawned backend restores a.py and serves hover.
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
    // Different-length pyvenv.cfg content: belt-and-braces against the
    // identity token missing an inode/mtime collision.
    std::fs::write(
        pkg_dir.join(".venv/pyvenv.cfg"),
        "home = /usr/bin\nversion = 3.12\n",
    )
    .unwrap();

    // A spurious extra respawn would double-consume life2's scenario steps
    // and fail this hover, so the assertion below also proves "exactly once".
    let (hover2, notifications) = proxy
        .request_collecting("textDocument/hover", hover_params)
        .await;
    assert!(
        hover2.error.is_none(),
        "hover after venv replacement should succeed, got error: {:?}",
        hover2.error
    );
    assert_eq!(
        hover2.result.as_ref().unwrap()["contents"]["value"],
        "hover life2"
    );

    // The replacement must be announced to the client.
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

/// E2E: a venv replacement whose respawn fails (broken interpreter, e.g.
/// mid-`uv sync`) must not kill the proxy. The `didChange` forward path is
/// the trigger here because it used to propagate the spawn error fatally.
/// The client is notified, subsequent requests get explicit errors, and once
/// the venv is healthy again the next request respawns and recovers.
#[tokio::test]
async fn failed_respawn_is_contained_and_recovers() {
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

    let hover_params = serde_json::json!({
        "textDocument": { "uri": &file_a_uri },
        "position": { "line": 0, "character": 0 }
    });

    let hover1 = proxy
        .request("textDocument/hover", hover_params.clone())
        .await;
    assert!(hover1.error.is_none());
    assert_eq!(
        hover1.result.as_ref().unwrap()["contents"]["value"],
        "hover life1"
    );

    // Replace the venv with a broken one: pyvenv.cfg present (new identity)
    // but the fake pyright-langserver exits immediately, so the respawn's
    // initialize handshake fails deterministically.
    std::fs::remove_dir_all(pkg_dir.join(".venv")).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    support::write_broken_venv_fixture(&pkg_dir);

    // didChange triggers the staleness check: mismatch -> evict -> respawn
    // fails. Before the containment fix this terminated the whole proxy.
    proxy.did_change(&file_a_uri, 2, "a = 2\n").await;

    let replaced = proxy
        .wait_for_notification("window/showMessage", 5000)
        .await;
    let replaced_msg = replaced.params.as_ref().unwrap()["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        replaced_msg.contains("replaced"),
        "first showMessage should announce the replaced venv, got: {replaced_msg}"
    );

    let failure = proxy
        .wait_for_notification("window/showMessage", 5000)
        .await;
    let failure_msg = failure.params.as_ref().unwrap()["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        failure_msg.contains("Failed to start LSP backend"),
        "second showMessage should report the failed respawn, got: {failure_msg}"
    );

    // The proxy must still be alive and answering. The venv is still broken,
    // so the lazy retry (pool-miss spawn) fails and the request gets an
    // explicit JSON-RPC error — not silence, not a dead proxy.
    let hover2 = proxy
        .request("textDocument/hover", hover_params.clone())
        .await;
    let error = hover2
        .error
        .expect("hover against the broken venv must fail explicitly");
    assert!(
        error.message.contains("backend error"),
        "expected a backend error response, got: {}",
        error.message
    );

    // Heal the venv. The next request retries backend creation and recovers.
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

    let hover3 = proxy.request("textDocument/hover", hover_params).await;
    assert!(
        hover3.error.is_none(),
        "hover after healing the venv should succeed, got error: {:?}",
        hover3.error
    );
    assert_eq!(
        hover3.result.as_ref().unwrap()["contents"]["value"],
        "hover life2"
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}

/// E2E: a `.venv` whose `pyvenv.cfg` disappears is served during the grace
/// window (transient `uv sync` gap), then evicted without respawn — strict
/// mode returns an explicit error instead of silently serving stale results.
#[tokio::test]
async fn removed_venv_evicts_after_grace_and_errors() {
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
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover before removal" } } }]
            },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover during grace" } } }]
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

    let hover_params = serde_json::json!({
        "textDocument": { "uri": &file_a_uri },
        "position": { "line": 0, "character": 0 }
    });

    let hover1 = proxy
        .request("textDocument/hover", hover_params.clone())
        .await;
    assert!(hover1.error.is_none());
    assert_eq!(
        hover1.result.as_ref().unwrap()["contents"]["value"],
        "hover before removal"
    );

    // Remove pyvenv.cfg only (mid-`uv sync` state). Past the 1s debounce
    // but well inside the 2s grace: the old backend still serves.
    std::fs::remove_file(pkg_dir.join(".venv/pyvenv.cfg")).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

    let hover2 = proxy
        .request("textDocument/hover", hover_params.clone())
        .await;
    assert!(
        hover2.error.is_none(),
        "hover within grace should still be served by the old backend, got error: {:?}",
        hover2.error
    );
    assert_eq!(
        hover2.result.as_ref().unwrap()["contents"]["value"],
        "hover during grace"
    );

    // Push past grace (2s since the missing mark) with margin: the backend
    // is evicted without respawn and strict mode reports the missing venv.
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;

    let hover3 = proxy.request("textDocument/hover", hover_params).await;
    let error = hover3
        .error
        .expect("hover after grace must fail: venv is gone (strict mode)");
    assert!(
        error.message.contains(".venv not found"),
        "expected strict-mode '.venv not found' error, got: {}",
        error.message
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}
