mod support;

use std::time::{Duration, Instant};
use support::{PackageConfig, ProxyUnderTest, WorkspaceConfig};

/// E2E: the harness redirects a spawned proxy's `HOME` (see #120), so
/// `~/.config/typemux-cc/config` resolves under a harness-controlled
/// directory rather than the developer's real one.
///
/// This is proven positively rather than by touching the real `$HOME`
/// (which would risk clobbering a developer's actual config): a config file
/// is planted at `<fake_home>/.config/typemux-cc/config` setting
/// `TYPEMUX_CC_WARMUP_TIMEOUT=0` (disables warmup, so an index-dependent
/// request is answered immediately instead of queued for up to the 2s
/// default). `HOME` is passed as an explicit `spawn_with_env` entry, so this
/// also doubles as the "explicit env wins over harness defaults" check for
/// `HOME` specifically (the harness's own default is a *different*,
/// empty per-spawn temp dir with no config file).
///
/// If `HOME` redirection were broken (e.g. the proxy fell back to
/// inheriting the test process's own real `HOME`), this fake config would
/// never be read: the proxy would use the hardcoded 2s default warmup
/// timeout, and the definition request below would only be answered after
/// that timeout's fail-open drain — comfortably tripping the 500ms budget
/// asserted below.
#[tokio::test]
async fn config_at_redirected_home_takes_effect() {
    let fake_home = tempfile::TempDir::new().expect("failed to create fake HOME dir");
    let config_dir = fake_home.path().join(".config").join("typemux-cc");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("config"), "TYPEMUX_CC_WARMUP_TIMEOUT=0\n").unwrap();

    let scenario = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [{ "type": "respond", "body": { "capabilities": { "hoverProvider": true, "definitionProvider": true } } }]
            },
            { "expect": { "method": "initialized" }, "actions": [] },
            // dispatch_initialized forwards a 2nd "initialized" to fallback backends
            { "expect": { "method": "initialized" }, "actions": [] },
            { "expect": { "method": "textDocument/didOpen" }, "actions": [] },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{ "type": "respond", "body": { "contents": { "kind": "plaintext", "value": "hover ok" } } }]
            },
            {
                "expect": { "method": "textDocument/definition" },
                "actions": [{ "type": "respond", "body": { "uri": "file:///a.py", "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } } } }]
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
    let mut proxy = ProxyUnderTest::spawn_with_env(
        temp_dir,
        root.clone(),
        &pkg_dir,
        &[("HOME", fake_home.path().to_str().unwrap())],
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

    // Synchronizing round trip: absorbs backend spawn latency outside the
    // timed window below, and confirms the backend is fully up.
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

    let start = Instant::now();
    let def_resp = proxy
        .request(
            "textDocument/definition",
            serde_json::json!({
                "textDocument": { "uri": &file_a_uri },
                "position": { "line": 0, "character": 0 }
            }),
        )
        .await;
    let elapsed = start.elapsed();

    assert!(
        def_resp.error.is_none(),
        "definition should not return an error, got: {:?}",
        def_resp.error
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "definition took {elapsed:?}, expected well under 500ms: \
         TYPEMUX_CC_WARMUP_TIMEOUT=0 at the redirected HOME's config should \
         have disabled warmup queueing entirely (a broken HOME redirect \
         would fall back to the crate's 2s default warmup timeout)"
    );

    let shutdown_resp = proxy.shutdown_and_exit().await;
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown should not return an error"
    );
}
