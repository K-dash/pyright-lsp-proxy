//! Mock LSP backend for E2E tests.
//!
//! Reads a JSON scenario from `MOCK_LSP_SCENARIO` (inline JSON) or
//! `MOCK_LSP_SCENARIO_FILE` (path), then plays it back over stdin/stdout
//! using the same LSP framing the real proxy uses.

use serde::Deserialize;
use serde_json::Value;
use std::process;
use tokio::io;
use typemux_cc::error::FramingError;
use typemux_cc::framing::{LspFrameReader, LspFrameWriter};
use typemux_cc::message::RpcMessage;

// ── Scenario types ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct Scenario {
    #[serde(default)]
    on_startup: Vec<Action>,
    #[serde(default)]
    steps: Vec<Step>,
}

#[derive(Debug, Deserialize)]
struct Step {
    expect: Expect,
    #[serde(default)]
    actions: Vec<Action>,
}

#[derive(Debug, Deserialize)]
struct Expect {
    method: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Action {
    Respond { body: Value },
    Notify { method: String, params: Value },
    SleepMs { ms: u64 },
    Crash,
    Eof,
}

// ── Main ────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let scenario = load_scenario();

    let mut reader = LspFrameReader::new(io::stdin());
    let mut writer = LspFrameWriter::new(io::stdout());

    // Execute on_startup actions before reading any messages.
    for action in &scenario.on_startup {
        execute_action(action, None, &mut writer).await;
    }

    // Step-by-step execution.
    for (i, step) in scenario.steps.iter().enumerate() {
        let msg = match reader.read_message().await {
            Ok(m) => m,
            Err(FramingError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                eprintln!(
                    "mock-lsp-backend: EOF at step {i} (expected method {:?}) — scenario incomplete",
                    step.expect.method
                );
                process::exit(1);
            }
            Err(e) => {
                eprintln!("mock-lsp-backend: read error at step {i}: {e}");
                process::exit(1);
            }
        };

        let got_method = msg.method_name().unwrap_or("<response>");
        if got_method != step.expect.method {
            eprintln!(
                "mock-lsp-backend: step {i}: expected method {:?}, got {:?}",
                step.expect.method, got_method
            );
            process::exit(1);
        }

        for action in &step.actions {
            execute_action(action, Some(&msg), &mut writer).await;
        }
    }

    // All steps consumed — drain remaining messages until EOF.
    // Allow shutdown/exit from the proxy's fire-and-forget cleanup;
    // any other message is unexpected and fails the test.
    loop {
        match reader.read_message().await {
            Ok(msg) => {
                let method = msg.method_name().unwrap_or("<response>");
                if method == "shutdown" || method == "exit" {
                    if msg.is_request() {
                        // Respond to shutdown request.
                        let resp = RpcMessage::success_response(&msg, serde_json::Value::Null);
                        writer.write_message(&resp).await.unwrap_or_else(|e| {
                            eprintln!("mock-lsp-backend: write error: {e}");
                            process::exit(1);
                        });
                    }
                    // exit notification → just continue draining.
                } else {
                    eprintln!("mock-lsp-backend: unexpected message after all steps: {method:?}");
                    process::exit(1);
                }
            }
            Err(FramingError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return;
            }
            Err(e) => {
                eprintln!("mock-lsp-backend: read error in drain loop: {e}");
                process::exit(1);
            }
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

fn load_scenario() -> Scenario {
    // Try inline JSON first, then file path.
    // A `map_or_else` rewrite here would nest an if/else inside a closure for no
    // readability gain over this plain three-way if/else-if/else.
    #[allow(clippy::option_if_let_else)]
    let json = if let Ok(s) = std::env::var("MOCK_LSP_SCENARIO") {
        s
    } else if let Ok(path) = std::env::var("MOCK_LSP_SCENARIO_FILE") {
        std::fs::read_to_string(&path).unwrap_or_else(|e| {
            eprintln!("mock-lsp-backend: cannot read scenario file {path:?}: {e}");
            process::exit(1);
        })
    } else {
        eprintln!("mock-lsp-backend: set MOCK_LSP_SCENARIO or MOCK_LSP_SCENARIO_FILE");
        process::exit(1);
    };

    serde_json::from_str(&json).unwrap_or_else(|e| {
        eprintln!("mock-lsp-backend: invalid scenario JSON: {e}");
        process::exit(1);
    })
}

async fn execute_action<W: tokio::io::AsyncWrite + Unpin>(
    action: &Action,
    request: Option<&RpcMessage>,
    writer: &mut LspFrameWriter<W>,
) {
    match action {
        Action::Respond { body } => {
            let req = request.expect("respond action requires a preceding request");
            let response = RpcMessage::success_response(req, body.clone());
            writer.write_message(&response).await.unwrap_or_else(|e| {
                eprintln!("mock-lsp-backend: write error: {e}");
                process::exit(1);
            });
        }
        Action::Notify { method, params } => {
            let notification = RpcMessage::notification(method, Some(params.clone()));
            writer
                .write_message(&notification)
                .await
                .unwrap_or_else(|e| {
                    eprintln!("mock-lsp-backend: write error: {e}");
                    process::exit(1);
                });
        }
        Action::SleepMs { ms } => {
            tokio::time::sleep(std::time::Duration::from_millis(*ms)).await;
        }
        Action::Crash => {
            process::exit(1);
        }
        Action::Eof => {
            // Close stdout and exit cleanly.
            drop(std::io::stdout());
            process::exit(0);
        }
    }
}
