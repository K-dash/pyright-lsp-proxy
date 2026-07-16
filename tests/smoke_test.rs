mod support;

use support::{PackageConfig, ProxyUnderTest, WorkspaceConfig};

/// Priority 1: Basic LSP lifecycle — initialize → initialized → didOpen →
/// hover → shutdown → exit.
///
/// `initialize` alone no longer spawns a backend (#140: it always answers
/// instantly with empty capabilities, before any venv-resolving message).
/// `didOpen` + a synchronizing hover are included so the mock scenario below
/// still proves the backend actually receives `initialize`/`initialized`
/// (via the lazy Creating path) and the shutdown handshake — without them
/// this test would pass vacuously, with no backend ever spawned.
#[tokio::test]
async fn smoke_test_lifecycle() {
    let scenario = serde_json::json!({
        "on_startup": [],
        "steps": [
            {
                "expect": { "method": "initialize" },
                "actions": [{
                    "type": "respond",
                    "body": {
                        "capabilities": {
                            "textDocumentSync": 1,
                            "hoverProvider": true
                        }
                    }
                }]
            },
            {
                "expect": { "method": "initialized" },
                "actions": []
            },
            {
                "expect": { "method": "textDocument/didOpen" },
                "actions": []
            },
            {
                "expect": { "method": "textDocument/hover" },
                "actions": [{
                    "type": "respond",
                    "body": { "contents": { "kind": "plaintext", "value": "hover ok" } }
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
    let mut proxy = ProxyUnderTest::spawn(temp_dir, root.clone(), &root.join("pkg"));

    // Initialize: instant empty-capabilities response (#140) — no backend
    // has been spawned yet, so this doesn't wait on any backend handshake.
    let root_uri = support::path_to_uri(&root.join("pkg"));
    let init_resp = proxy.initialize(&root_uri).await;
    assert!(
        init_resp.result.is_some(),
        "initialize should return a result"
    );
    assert_eq!(
        init_resp.result.as_ref().unwrap()["capabilities"],
        serde_json::json!({}),
        "capabilities must be the frozen empty object, regardless of the backend's own capabilities"
    );

    // Initialized
    proxy.send_initialized().await;

    // didOpen triggers lazy backend creation; hover is the synchronizing
    // round trip proving the backend actually completed the
    // initialize/initialized handshake scripted into the mock scenario
    // above, through the ordinary Creating → Ready path.
    let file_a = root.join("pkg/a.py");
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
        "hover ok",
        "hover response must come from the backend, proving initialize/initialized reached it"
    );

    // Shutdown
    let shutdown_resp = proxy.shutdown_and_exit().await;
    // serde deserializes `"result": null` into `None` for Option<Value>,
    // so we check that the response is not an error rather than matching the exact value.
    assert!(
        shutdown_resp.error.is_none(),
        "shutdown response should not be an error"
    );
}
