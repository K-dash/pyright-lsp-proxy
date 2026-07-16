mod support;

use std::time::{Duration, Instant};
use support::{PackageConfig, ProxyUnderTest, WorkspaceConfig};

/// Regression test for #140: a fallback `.venv` present at startup must
/// never be used to spawn a backend synchronously inside `initialize` —
/// that path permanently wedged Claude Code 2.1.207's LSP client. Every
/// startup now answers `initialize` instantly with empty capabilities, and
/// the first backend, like every other, is created lazily by the Creating
/// machinery on the first venv-resolving client message.
///
/// The fake `pyright-langserver` is instrumented to append to a spawn-marker
/// file at process startup (before it even reads the mock scenario) and its
/// scripted `initialize` handshake is deliberately slow (3s), so:
///
/// (i) the client's `initialize` response arriving well under that delay is
///     direct proof it never waited on any backend handshake;
/// (ii) the marker's absence after `initialize` + `initialized` + a settle
///      delay is direct proof no backend process was launched at all yet;
/// (iii) after `didOpen` + hover, the marker exists (creation happened) and
///       the hover is answered only once creation (including the slow
///       handshake) completes — queued during Creating, replayed after.
#[tokio::test]
async fn initialize_is_instant_and_backend_spawns_lazily() {
    let scenario = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [
                    { "type": "sleep_ms", "ms": 3000 },
                    { "type": "respond", "body": { "capabilities": { "hoverProvider": true } } }
                ]
            },
            { "expect": { "method": "initialized" }, "actions": [] },
            { "expect": { "method": "textDocument/didOpen" }, "actions": [] },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{
                    "type": "respond",
                    "body": { "contents": { "kind": "plaintext", "value": "hover after creation" } }
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
            name: "pkg".to_string(),
            scenario,
            has_venv: true,
        }],
    };

    let (temp_dir, root) = support::setup_test_workspace(&config);
    let pkg_dir = root.join("pkg");

    // Instrument the fake `pyright-langserver` to record every spawn, at
    // process startup, before it even reads the scenario file.
    let spawn_marker = root.join("spawn-marker.txt");
    instrument_venv_spawn_marker(&pkg_dir, &spawn_marker);

    // cwd = pkg_dir: a `.venv` is present at startup, so the (now
    // diagnostics-only) fallback search finds it — the case #140 wedged on.
    let mut proxy = ProxyUnderTest::spawn(temp_dir, root.clone(), &pkg_dir);

    // (i) initialize answers instantly, well under the shim's 3s handshake
    // delay — proves no backend handshake is awaited synchronously here.
    // The 1.5s margin absorbs this harness's own process-spawn jitter (a few
    // hundred ms observed) while staying well clear of the 3s shim delay a
    // synchronous pre-spawn would have blocked on.
    let root_uri = support::path_to_uri(&pkg_dir);
    let start = Instant::now();
    let init_resp = proxy.initialize(&root_uri).await;
    let elapsed = start.elapsed();
    assert!(
        init_resp.error.is_none(),
        "initialize should not return an error"
    );
    assert_eq!(
        init_resp.result.as_ref().unwrap()["capabilities"],
        serde_json::json!({}),
        "capabilities must be the frozen empty object"
    );
    assert!(
        elapsed < Duration::from_millis(1500),
        "initialize took {elapsed:?}, expected well under the shim's 3s handshake delay: \
         a synchronous pre-spawn would have blocked on it"
    );

    proxy.send_initialized().await;

    // (ii) settle delay: the spawn marker is written at process startup, so
    // anything spawned by now would have left one. Still absent — direct
    // proof nothing was launched before a venv-resolving message arrived.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !spawn_marker.exists(),
        "no backend process should have been spawned yet"
    );

    // (iii) didOpen starts lazy creation; hover queues behind it (Creating)
    // and is answered once creation — including the shim's slow handshake —
    // completes.
    let file_a = pkg_dir.join("a.py");
    std::fs::write(&file_a, "a = 1\n").unwrap();
    let file_a_uri = support::path_to_uri(&file_a);
    proxy.did_open(&file_a_uri, "a = 1\n").await;

    let hover = proxy
        .request(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": &file_a_uri },
                "position": { "line": 0, "character": 0 }
            }),
        )
        .await;
    assert!(
        hover.error.is_none(),
        "hover should not return an error, got: {:?}",
        hover.error
    );
    assert_eq!(
        hover.result.as_ref().unwrap()["contents"]["value"],
        "hover after creation"
    );
    assert!(
        spawn_marker.exists(),
        "the backend must have been spawned by now, triggered by the first venv-resolving message"
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}

/// Rewrite `<pkg_dir>/.venv/bin/pyright-langserver` (already created by
/// `support::write_venv_fixture`) to append a line to `marker` on every
/// invocation before delegating to `mock-lsp-backend`, so the test can prove
/// exactly when (and whether) a backend process was actually spawned.
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
