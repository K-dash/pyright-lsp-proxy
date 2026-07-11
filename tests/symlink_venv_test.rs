mod support;

use support::{PackageConfig, ProxyUnderTest, WorkspaceConfig};

/// Regression test for #95: `find_venv` must resolve `.venv` when the
/// client's file URI reaches the project through a symlink that disagrees
/// with the (already-canonicalized) git toplevel — the macOS `/tmp` ->
/// `/private/tmp` case is the concrete example, reproduced here with an
/// explicit symlink so it also exercises on Linux CI.
///
/// Also covers pool-key uniqueness: the same physical venv is reached via
/// two textually different URIs (symlinked and physical), and a spawn
/// counter embedded in the fake `pyright-langserver` script asserts only
/// one backend process is ever spawned for it.
#[tokio::test]
async fn symlinked_project_path_resolves_venv_and_dedupes_pool_key() {
    let scenario = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [{
                    "type": "respond",
                    "body": { "capabilities": { "hoverProvider": true } }
                }]
            },
            { "expect": { "method": "initialized" }, "actions": [] },
            { "expect": { "method": "textDocument/didOpen" }, "actions": [] },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{
                    "type": "respond",
                    "body": { "contents": { "kind": "plaintext", "value": "via symlink" } }
                }]
            },
            { "expect": { "method": "textDocument/didOpen" }, "actions": [] },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{
                    "type": "respond",
                    "body": { "contents": { "kind": "plaintext", "value": "via physical" } }
                }]
            },
            {
                "expect": { "method": "shutdown" },
                "actions": [{ "type": "respond", "body": null }]
            }
        ]
    });

    let config = WorkspaceConfig {
        packages: vec![PackageConfig {
            name: "pkg-a".to_string(),
            scenario,
            has_venv: true,
        }],
    };

    let (temp_dir, root) = support::setup_test_workspace(&config);
    let pkg_dir = root.join("pkg-a");

    // Instrument the fake `pyright-langserver` to record every spawn, so we
    // can assert the pool never spawns a second backend for the logical vs.
    // physical alias of the same venv.
    let spawn_marker = root.join("spawn-marker.txt");
    instrument_venv_spawn_marker(&pkg_dir, &spawn_marker);

    // Symlink to the workspace root — the project reached through it has a
    // different textual path from the (canonical) git toplevel, mirroring a
    // client URI built from a logical path like macOS's /tmp -> /private/tmp.
    let symlink_root = temp_dir.path().join("workspace-symlink");
    std::os::unix::fs::symlink(&root, &symlink_root).unwrap();

    let file_physical = pkg_dir.join("main.py");
    std::fs::write(&file_physical, "x = 1\n").unwrap();

    let mut proxy = ProxyUnderTest::spawn_with_env(
        temp_dir,
        root.clone(),
        &root,
        &[("RUST_LOG", "typemux_cc=warn")],
    );

    let root_uri = support::path_to_uri(&root);
    let init_resp = proxy.initialize(&root_uri).await;
    assert!(
        init_resp.error.is_none(),
        "initialize should not return an error"
    );
    proxy.send_initialized().await;

    // didOpen + hover via the symlinked (logical) path.
    let file_symlink_uri = support::path_to_uri(&symlink_root.join("pkg-a/main.py"));
    proxy.did_open(&file_symlink_uri, "x = 1\n").await;

    let hover_via_symlink = proxy
        .request(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": file_symlink_uri },
                "position": { "line": 0, "character": 0 }
            }),
        )
        .await;
    assert!(
        hover_via_symlink.error.is_none(),
        "hover through the symlinked path should resolve the venv and succeed, got error: {:?}",
        hover_via_symlink.error
    );

    // didOpen + hover via the physical path (same underlying file, same venv).
    let file_physical_uri = support::path_to_uri(&file_physical);
    proxy.did_open(&file_physical_uri, "x = 1\n").await;

    let hover_via_physical = proxy
        .request(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": file_physical_uri },
                "position": { "line": 0, "character": 0 }
            }),
        )
        .await;
    assert!(
        hover_via_physical.error.is_none(),
        "hover through the physical path should reuse the same pooled backend, got error: {:?}",
        hover_via_physical.error
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );

    // Both requests were served by the mock backend's single sequential step
    // list (one didOpen+hover pair per alias) — if a second backend had been
    // spawned under a distinct (non-canonical) pool key, the scenario's
    // strict step ordering above would have failed instead of the spawn
    // count below.
    let spawn_count = std::fs::read_to_string(&spawn_marker).map_or(0, |s| s.lines().count());
    assert_eq!(
        spawn_count, 1,
        "expected exactly one backend spawn for the venv shared by the symlinked and physical aliases"
    );
}

/// Rewrite `<pkg_dir>/.venv/bin/pyright-langserver` (already created by
/// `support::write_venv_fixture`) to append a line to `marker` on every
/// invocation before delegating to `mock-lsp-backend`, so the test can
/// count how many backend processes were actually spawned for the venv.
fn instrument_venv_spawn_marker(pkg_dir: &std::path::Path, marker: &std::path::Path) {
    let mock_backend_bin = env!("CARGO_BIN_EXE_mock-lsp-backend");
    let script_path = pkg_dir.join(".venv/bin/pyright-langserver");
    let script = format!(
        "#!/bin/sh\necho spawned >> \"{}\"\nexport MOCK_LSP_SCENARIO_FILE=\"$VIRTUAL_ENV/scenario.json\"\nexec \"{}\" \"$@\"\n",
        marker.display(),
        mock_backend_bin
    );
    std::fs::write(&script_path, script).unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}
