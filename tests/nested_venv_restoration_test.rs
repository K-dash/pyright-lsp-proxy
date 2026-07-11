mod support;

use support::{PackageConfig, ProxyUnderTest, WorkspaceConfig};

/// E2E: nested venv layout (`parent/.venv` + `parent/child/.venv`).
/// Respawning the parent backend must not replay the child-owned document
/// into it — the old `starts_with` restoration fallback fired on any
/// document under the venv's project root, regardless of which venv
/// actually owned it (#94).
#[tokio::test]
async fn nested_venv_respawn_excludes_child_owned_document() {
    let scenario_parent_life1 = serde_json::json!({
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

    let scenario_child = serde_json::json!({
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
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover from child" } } }]
            },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover from child" } } }]
            },
            {
                "expect": { "method": "shutdown" },
                "actions": [{ "type": "respond", "body": null }]
            }
        ]
    });

    let config = WorkspaceConfig {
        packages: vec![PackageConfig {
            name: "parent".to_string(),
            scenario: scenario_parent_life1,
            has_venv: true,
        }],
    };

    let (temp_dir, root) = support::setup_test_workspace(&config);
    let parent_dir = root.join("parent");
    let child_dir = parent_dir.join("child");
    support::write_venv_fixture(&child_dir, &scenario_child);

    let mut proxy = ProxyUnderTest::spawn_with_env(
        temp_dir,
        root.clone(),
        &root,
        &[
            ("TYPEMUX_CC_VENV_CHECK_INTERVAL", "1"),
            ("RUST_LOG", "typemux_cc=warn"),
        ],
    );

    let root_uri = support::path_to_uri(&root);
    let init_resp = proxy.initialize(&root_uri).await;
    assert!(
        init_resp.error.is_none(),
        "initialize should not return an error"
    );
    proxy.send_initialized().await;

    // Parent-owned document: resolves to `parent/.venv`.
    let parent_file = parent_dir.join("app.py");
    std::fs::write(&parent_file, "a = 1\n").unwrap();
    let parent_file_uri = support::path_to_uri(&parent_file);
    proxy.did_open(&parent_file_uri, "a = 1\n").await;

    let hover_params = |uri: &str| {
        serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": 0, "character": 0 }
        })
    };

    // Synchronize on a hover response before touching `.venv`: `didOpen` is
    // fire-and-forget, so without this round trip the parent backend might
    // not have finished spawning yet when the test deletes `parent/.venv`
    // below, letting `pyright-langserver` resolve to a real system install
    // instead of the fixture's mock shim.
    let hover1 = proxy
        .request("textDocument/hover", hover_params(&parent_file_uri))
        .await;
    assert!(hover1.error.is_none(), "hover on parent should succeed");
    assert_eq!(
        hover1.result.as_ref().unwrap()["contents"]["value"],
        "hover life1"
    );

    // Child-owned document: nested under the parent's project root, but
    // resolves to its own closer `.venv` (`parent/child/.venv`).
    let child_file = child_dir.join("app.py");
    std::fs::write(&child_file, "b = 2\n").unwrap();
    let child_file_uri = support::path_to_uri(&child_file);
    proxy.did_open(&child_file_uri, "b = 2\n").await;

    let child_hover1 = proxy
        .request("textDocument/hover", hover_params(&child_file_uri))
        .await;
    assert!(
        child_hover1.error.is_none(),
        "hover on child should succeed"
    );
    assert_eq!(
        child_hover1.result.as_ref().unwrap()["contents"]["value"],
        "hover from child"
    );

    // Force a respawn of the PARENT backend only (simulate `uv sync` on
    // `parent/.venv`); the child's `.venv` is left untouched.
    std::fs::remove_dir_all(parent_dir.join(".venv")).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

    let scenario_parent_life2 = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [{ "type": "respond", "body": { "capabilities": { "hoverProvider": true } } }]
            },
            { "expect": { "method": "initialized" }, "actions": [] },
            // Only the parent's own document restores here. A spurious
            // restore of the child-owned document would consume this slot
            // as a second `didOpen` and desync the `hover` step below,
            // making the mock backend exit deterministically.
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
    support::write_venv_fixture(&parent_dir, &scenario_parent_life2);
    // Different-length pyvenv.cfg content: belt-and-braces against the
    // identity token missing an inode/mtime collision.
    std::fs::write(
        parent_dir.join(".venv/pyvenv.cfg"),
        "home = /usr/bin\nversion = 3.12\n",
    )
    .unwrap();

    let hover_after_respawn = proxy
        .request("textDocument/hover", hover_params(&parent_file_uri))
        .await;
    assert!(
        hover_after_respawn.error.is_none(),
        "hover against the respawned parent backend should succeed, got error: {:?}",
        hover_after_respawn.error
    );
    assert_eq!(
        hover_after_respawn.result.as_ref().unwrap()["contents"]["value"],
        "hover life2"
    );

    // The child backend must be untouched by the parent's respawn: it still
    // answers with its own scenario, proving the child-owned document was
    // never duplicated into (or disturbed via) the parent backend.
    let child_hover2 = proxy
        .request("textDocument/hover", hover_params(&child_file_uri))
        .await;
    assert!(
        child_hover2.error.is_none(),
        "hover against the child backend should succeed, got error: {:?}",
        child_hover2.error
    );
    assert_eq!(
        child_hover2.result.as_ref().unwrap()["contents"]["value"],
        "hover from child"
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}

/// E2E: a document whose venv was never resolved at `didOpen` (e.g. the
/// `.venv` did not exist yet) still restores into a backend respawned for
/// the venv that later ends up owning its project root — the `doc.venv ==
/// None` fallback kept in the restoration filter (#94's acceptance
/// criterion: "documents with venv: None under the project root still
/// restore").
#[tokio::test]
async fn none_venv_document_still_restores_on_respawn() {
    let config = WorkspaceConfig {
        packages: vec![PackageConfig {
            name: "parent".to_string(),
            scenario: serde_json::json!({ "on_startup": [], "steps": [] }),
            has_venv: false,
        }],
    };

    let (temp_dir, root) = support::setup_test_workspace(&config);
    let parent_dir = root.join("parent");

    let mut proxy = ProxyUnderTest::spawn_with_env(
        temp_dir,
        root.clone(),
        &root,
        &[
            ("TYPEMUX_CC_VENV_CHECK_INTERVAL", "1"),
            ("RUST_LOG", "typemux_cc=warn"),
        ],
    );

    let root_uri = support::path_to_uri(&root);
    let init_resp = proxy.initialize(&root_uri).await;
    assert!(
        init_resp.error.is_none(),
        "initialize should not return an error"
    );
    proxy.send_initialized().await;

    // Open a document before any `.venv` exists under `parent/`: venv
    // resolution fails at didOpen, so it's cached with `venv: None` and
    // never forwarded to any backend.
    let orphan_file = parent_dir.join("orphan.py");
    std::fs::write(&orphan_file, "o = 1\n").unwrap();
    let orphan_file_uri = support::path_to_uri(&orphan_file);
    proxy.did_open(&orphan_file_uri, "o = 1\n").await;

    // Now create `parent/.venv` and open a second, venv-resolved document
    // that spawns the backend.
    let scenario_life1 = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [{ "type": "respond", "body": { "capabilities": { "hoverProvider": true } } }]
            },
            { "expect": { "method": "initialized" }, "actions": [] },
            // The orphan document (`venv: None`) already sits in
            // `open_documents` by the time this backend spawns, so it
            // restores here too, alongside the anchor's own `didOpen` —
            // not just on the later respawn. Order between the two is not
            // guaranteed (see the life2 scenario below for why).
            { "expect": { "method": "textDocument/didOpen" }, "actions": [] },
            { "expect": { "method": "textDocument/didOpen" }, "actions": [] },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover life1" } } }]
            }
        ]
    });
    support::write_venv_fixture(&parent_dir, &scenario_life1);

    let anchor_file = parent_dir.join("anchor.py");
    std::fs::write(&anchor_file, "x = 1\n").unwrap();
    let anchor_file_uri = support::path_to_uri(&anchor_file);
    proxy.did_open(&anchor_file_uri, "x = 1\n").await;

    let hover_params = |uri: &str| {
        serde_json::json!({
            "textDocument": { "uri": uri },
            "position": { "line": 0, "character": 0 }
        })
    };

    // Synchronize on a hover response before touching `.venv` (see the
    // sibling test for why this round trip is required, not optional).
    let hover1 = proxy
        .request("textDocument/hover", hover_params(&anchor_file_uri))
        .await;
    assert!(
        hover1.error.is_none(),
        "hover on anchor should succeed, got error: {:?}",
        hover1.error
    );
    assert_eq!(
        hover1.result.as_ref().unwrap()["contents"]["value"],
        "hover life1"
    );

    // Force a respawn of the backend (simulate `uv sync`).
    std::fs::remove_dir_all(parent_dir.join(".venv")).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

    let scenario_life2 = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [{ "type": "respond", "body": { "capabilities": { "hoverProvider": true } } }]
            },
            { "expect": { "method": "initialized" }, "actions": [] },
            // The mock backend only matches on method name, not on which
            // document a `didOpen` carries, so these two steps accept the
            // anchor's (`venv == Some`) and the orphan's (`venv == None`)
            // restorations in either order. Exactly two must arrive.
            { "expect": { "method": "textDocument/didOpen" }, "actions": [] },
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
    support::write_venv_fixture(&parent_dir, &scenario_life2);
    std::fs::write(
        parent_dir.join(".venv/pyvenv.cfg"),
        "home = /usr/bin\nversion = 3.12\n",
    )
    .unwrap();

    // Trigger the staleness check via the anchor document, which never
    // touches the orphan's cached `venv: None` entry — unlike a request for
    // the orphan itself, which would re-resolve and set it before restore
    // even runs.
    let hover_after_respawn = proxy
        .request("textDocument/hover", hover_params(&anchor_file_uri))
        .await;
    assert!(
        hover_after_respawn.error.is_none(),
        "hover against the respawned backend should succeed, got error: {:?}",
        hover_after_respawn.error
    );
    assert_eq!(
        hover_after_respawn.result.as_ref().unwrap()["contents"]["value"],
        "hover life2"
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}
