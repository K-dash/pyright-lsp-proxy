mod support;

use support::{PackageConfig, ProxyUnderTest, WorkspaceConfig};

/// `RUST_LOG` is pinned so a developer's `~/.config/typemux-cc/config` (which
/// may set `RUST_LOG=typemux_cc=trace`) can't slow down message processing
/// enough to make the bounded `drain_notifications` windows below flaky.
const ENV: &[(&str, &str)] = &[("RUST_LOG", "typemux_cc=warn")];

/// E2E: `textDocument/didSave` for a proj-a document must reach only
/// proj-a's backend.
///
/// proj-b's mock backend has no scripted step for a notification after its
/// `didOpen` — if `didSave` leaks to it (the pre-fix broadcast bug), it
/// treats the notification as an unexpected message and exits, which the
/// proxy's crash-cleanup path surfaces as an unprompted, empty
/// `textDocument/publishDiagnostics` for proj-b's document. Since a
/// `didSave` notification carries no request id, that crash has no pending
/// request to cancel — so absence of that spurious notification is the
/// only reliable signal, and it is checked in a bounded window before
/// issuing any further request (a later request would auto-respawn the
/// crashed backend and mask the crash).
#[tokio::test]
// `file_a`/`file_b` (and their `_uri` variants) are deliberately parallel names
// for the two test fixtures this scenario exercises.
#[allow(clippy::similar_names)]
async fn did_save_routes_only_to_owning_backend() {
    let scenario_a = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [{ "type": "respond", "body": { "capabilities": { "hoverProvider": true } } }]
            },
            { "expect": { "method": "initialized" }, "actions": [] },
            { "expect": { "method": "textDocument/didOpen" }, "actions": [] },
            { "expect": { "method": "textDocument/didSave" }, "actions": [] },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover from backend-a after save" } } }]
            }
        ]
    });

    // No step beyond didOpen: any further message (e.g. a leaked didSave)
    // falls into the mock's drain loop, which treats it as unexpected.
    let scenario_b = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [{ "type": "respond", "body": { "capabilities": { "hoverProvider": true } } }]
            },
            { "expect": { "method": "initialized" }, "actions": [] },
            { "expect": { "method": "textDocument/didOpen" }, "actions": [] }
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
    let mut proxy = ProxyUnderTest::spawn_with_env(temp_dir, root.clone(), &root, ENV);

    let root_uri = support::path_to_uri(&root);
    let init_resp = proxy.initialize(&root_uri).await;
    assert!(
        init_resp.error.is_none(),
        "initialize should not return an error"
    );
    proxy.send_initialized().await;

    // didOpen for proj-a and proj-b → spawns both backends.
    let file_a = root.join("proj-a/main.py");
    std::fs::write(&file_a, "a = 1\n").unwrap();
    let file_a_uri = support::path_to_uri(&file_a);
    proxy.did_open(&file_a_uri, "a = 1\n").await;

    let file_b = root.join("proj-b/main.py");
    std::fs::write(&file_b, "b = 2\n").unwrap();
    let file_b_uri = support::path_to_uri(&file_b);
    proxy.did_open(&file_b_uri, "b = 2\n").await;

    // Save only proj-a's document.
    proxy
        .notify(
            "textDocument/didSave",
            serde_json::json!({ "textDocument": { "uri": &file_a_uri } }),
        )
        .await;

    // Bounded window for the crash-cleanup side effect to surface before it
    // can be masked by a later request auto-respawning proj-b's backend.
    let leaked = proxy.drain_notifications(500).await;
    assert!(
        leaked
            .iter()
            .all(|m| m.method.as_deref() != Some("textDocument/publishDiagnostics")),
        "unexpected publishDiagnostics after didSave(proj-a) — proj-b's backend likely \
         crashed from a spuriously delivered notification: {leaked:?}"
    );

    // proj-a must have consumed the didSave in order: hover only succeeds if
    // the scenario's didOpen -> didSave -> hover sequence matched exactly.
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
        "hover on proj-a should succeed after didSave, got error: {:?}",
        hover_a.error
    );
    assert_eq!(
        hover_a.result.as_ref().unwrap()["contents"]["value"],
        "hover from backend-a after save"
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}

/// E2E: a notification without `textDocument.uri` (e.g.
/// `workspace/didChangeConfiguration`) still reaches every pooled backend.
#[tokio::test]
// `file_a`/`file_b` (and their `_uri` variants) are deliberately parallel names
// for the two test fixtures this scenario exercises.
#[allow(clippy::similar_names)]
async fn non_uri_notification_still_broadcasts() {
    let scenario_a = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [{ "type": "respond", "body": { "capabilities": { "hoverProvider": true } } }]
            },
            { "expect": { "method": "initialized" }, "actions": [] },
            { "expect": { "method": "textDocument/didOpen" }, "actions": [] },
            { "expect": { "method": "workspace/didChangeConfiguration" }, "actions": [] },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover from backend-a after config" } } }]
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
            { "expect": { "method": "workspace/didChangeConfiguration" }, "actions": [] },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover from backend-b after config" } } }]
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
    let mut proxy = ProxyUnderTest::spawn_with_env(temp_dir, root.clone(), &root, ENV);

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

    // No `textDocument` field: must broadcast to every pooled backend.
    proxy
        .notify(
            "workspace/didChangeConfiguration",
            serde_json::json!({ "settings": {} }),
        )
        .await;

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
        "hover on proj-a should succeed after broadcast, got error: {:?}",
        hover_a.error
    );
    assert_eq!(
        hover_a.result.as_ref().unwrap()["contents"]["value"],
        "hover from backend-a after config"
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
        "hover on proj-b should succeed after broadcast, got error: {:?}",
        hover_b.error
    );
    assert_eq!(
        hover_b.result.as_ref().unwrap()["contents"]["value"],
        "hover from backend-b after config"
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}

/// E2E: `textDocument/didSave` for a document that was never opened is
/// dropped without crashing the proxy or the (only) pooled backend.
#[tokio::test]
async fn did_save_for_unopened_document_is_dropped() {
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
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover from pkg" } } }]
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
    let mut proxy = ProxyUnderTest::spawn_with_env(temp_dir, root.clone(), &root, ENV);

    let root_uri = support::path_to_uri(&root);
    let init_resp = proxy.initialize(&root_uri).await;
    assert!(
        init_resp.error.is_none(),
        "initialize should not return an error"
    );
    proxy.send_initialized().await;

    let file = root.join("pkg/main.py");
    std::fs::write(&file, "x = 1\n").unwrap();
    let file_uri = support::path_to_uri(&file);
    proxy.did_open(&file_uri, "x = 1\n").await;

    // A URI that was never opened: no entry in the open-documents cache, so
    // no owning backend can be resolved.
    let never_opened_uri = support::path_to_uri(&root.join("pkg/never_opened.py"));
    proxy
        .notify(
            "textDocument/didSave",
            serde_json::json!({ "textDocument": { "uri": &never_opened_uri } }),
        )
        .await;

    // Bounded window for a crash-cleanup side effect to surface before a
    // later request could auto-respawn the backend and mask a crash.
    let leaked = proxy.drain_notifications(500).await;
    assert!(
        leaked
            .iter()
            .all(|m| m.method.as_deref() != Some("textDocument/publishDiagnostics")),
        "unexpected publishDiagnostics after didSave for an unopened document — the \
         notification was likely broadcast instead of dropped: {leaked:?}"
    );

    // The pooled backend must still be alive and unperturbed.
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
        "hover should succeed after the dropped didSave, got error: {:?}",
        hover.error
    );
    assert_eq!(
        hover.result.as_ref().unwrap()["contents"]["value"],
        "hover from pkg"
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}
