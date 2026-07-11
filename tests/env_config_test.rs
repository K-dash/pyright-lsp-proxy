//! Startup validation for the timeout/interval env vars in `backend_pool.rs`.
//!
//! Unlike `TYPEMUX_CC_BACKEND`/`TYPEMUX_CC_MAX_BACKENDS`/etc., which go through
//! clap and already abort on invalid values, these three used to swallow parse
//! failures via `.ok()` and silently fall back to the default (issue #102).

use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn invalid_warmup_timeout_fails_startup() {
    let mut cmd = Command::cargo_bin("typemux-cc").unwrap();
    cmd.env("TYPEMUX_CC_WARMUP_TIMEOUT", "abc");

    cmd.assert().failure().stderr(
        predicate::str::contains("TYPEMUX_CC_WARMUP_TIMEOUT").and(predicate::str::contains("abc")),
    );
}

#[test]
fn invalid_fanout_timeout_fails_startup() {
    let mut cmd = Command::cargo_bin("typemux-cc").unwrap();
    cmd.env("TYPEMUX_CC_FANOUT_TIMEOUT", "5s");

    cmd.assert().failure().stderr(
        predicate::str::contains("TYPEMUX_CC_FANOUT_TIMEOUT").and(predicate::str::contains("5s")),
    );
}

#[test]
fn invalid_venv_check_interval_fails_startup() {
    let mut cmd = Command::cargo_bin("typemux-cc").unwrap();
    cmd.env("TYPEMUX_CC_VENV_CHECK_INTERVAL", "-1");

    cmd.assert().failure().stderr(
        predicate::str::contains("TYPEMUX_CC_VENV_CHECK_INTERVAL")
            .and(predicate::str::contains("-1")),
    );
}

#[test]
fn invalid_init_handshake_timeout_fails_startup() {
    let mut cmd = Command::cargo_bin("typemux-cc").unwrap();
    cmd.env("TYPEMUX_CC_INIT_HANDSHAKE_TIMEOUT", "soon");

    cmd.assert().failure().stderr(
        predicate::str::contains("TYPEMUX_CC_INIT_HANDSHAKE_TIMEOUT")
            .and(predicate::str::contains("soon")),
    );
}

#[test]
fn invalid_pool_sweep_interval_fails_startup() {
    let mut cmd = Command::cargo_bin("typemux-cc").unwrap();
    cmd.env("TYPEMUX_CC_POOL_SWEEP_INTERVAL", "abc");

    cmd.assert().failure().stderr(
        predicate::str::contains("TYPEMUX_CC_POOL_SWEEP_INTERVAL")
            .and(predicate::str::contains("abc")),
    );
}

/// Unlike the other three vars, `0` is a value-level error here, not just an
/// unparseable one: it would silently disable both the TTL sweep and the
/// venv staleness sweep (issue #126).
#[test]
fn zero_pool_sweep_interval_fails_startup() {
    let mut cmd = Command::cargo_bin("typemux-cc").unwrap();
    cmd.env("TYPEMUX_CC_POOL_SWEEP_INTERVAL", "0");

    cmd.assert().failure().stderr(
        predicate::str::contains("TYPEMUX_CC_POOL_SWEEP_INTERVAL")
            .and(predicate::str::contains("0")),
    );
}

#[test]
fn doctor_reports_invalid_env_value_distinctly() {
    let mut cmd = Command::cargo_bin("typemux-cc").unwrap();
    cmd.arg("--doctor").env("TYPEMUX_CC_WARMUP_TIMEOUT", "abc");

    // --doctor must not abort on an invalid value; it should surface the raw
    // value and mark it invalid instead of reporting the silent default.
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("\"abc\" (invalid)"))
        .stdout(predicate::str::contains(
            "env: TYPEMUX_CC_WARMUP_TIMEOUT (invalid",
        ));
}

#[test]
fn doctor_distinguishes_default_valid_and_invalid_env_values() {
    // default: unset
    let mut cmd = Command::cargo_bin("typemux-cc").unwrap();
    cmd.arg("--doctor").arg("--json");
    let default_output = cmd.output().expect("failed to run");
    assert!(default_output.status.success());
    let default_json: serde_json::Value = serde_json::from_slice(&default_output.stdout).unwrap();
    let default_item = find_config_item(&default_json, "fanout_timeout");
    assert_eq!(default_item["source"], "default");
    assert_eq!(default_item["value"], "5");

    // valid env value
    let mut cmd = Command::cargo_bin("typemux-cc").unwrap();
    cmd.arg("--doctor")
        .arg("--json")
        .env("TYPEMUX_CC_FANOUT_TIMEOUT", "9");
    let valid_output = cmd.output().expect("failed to run");
    assert!(valid_output.status.success());
    let valid_json: serde_json::Value = serde_json::from_slice(&valid_output.stdout).unwrap();
    let valid_item = find_config_item(&valid_json, "fanout_timeout");
    assert_eq!(valid_item["source"], "env: TYPEMUX_CC_FANOUT_TIMEOUT");
    assert_eq!(valid_item["value"], "9");

    // invalid env value
    let mut cmd = Command::cargo_bin("typemux-cc").unwrap();
    cmd.arg("--doctor")
        .arg("--json")
        .env("TYPEMUX_CC_FANOUT_TIMEOUT", "5s");
    let invalid_output = cmd.output().expect("failed to run");
    assert!(invalid_output.status.success());
    let invalid_json: serde_json::Value = serde_json::from_slice(&invalid_output.stdout).unwrap();
    let invalid_item = find_config_item(&invalid_json, "fanout_timeout");
    assert!(invalid_item["source"].as_str().unwrap().contains("invalid"));
    assert!(invalid_item["value"].as_str().unwrap().contains("5s"));
}

#[test]
fn doctor_distinguishes_default_valid_and_invalid_pool_sweep_interval() {
    // default: unset
    let mut cmd = Command::cargo_bin("typemux-cc").unwrap();
    cmd.arg("--doctor").arg("--json");
    let default_output = cmd.output().expect("failed to run");
    assert!(default_output.status.success());
    let default_json: serde_json::Value = serde_json::from_slice(&default_output.stdout).unwrap();
    let default_item = find_config_item(&default_json, "pool_sweep_interval");
    assert_eq!(default_item["source"], "default");
    assert_eq!(default_item["value"], "60");

    // valid env value
    let mut cmd = Command::cargo_bin("typemux-cc").unwrap();
    cmd.arg("--doctor")
        .arg("--json")
        .env("TYPEMUX_CC_POOL_SWEEP_INTERVAL", "5");
    let valid_output = cmd.output().expect("failed to run");
    assert!(valid_output.status.success());
    let valid_json: serde_json::Value = serde_json::from_slice(&valid_output.stdout).unwrap();
    let valid_item = find_config_item(&valid_json, "pool_sweep_interval");
    assert_eq!(valid_item["source"], "env: TYPEMUX_CC_POOL_SWEEP_INTERVAL");
    assert_eq!(valid_item["value"], "5");

    // invalid: unparseable
    let mut cmd = Command::cargo_bin("typemux-cc").unwrap();
    cmd.arg("--doctor")
        .arg("--json")
        .env("TYPEMUX_CC_POOL_SWEEP_INTERVAL", "5s");
    let invalid_output = cmd.output().expect("failed to run");
    assert!(invalid_output.status.success());
    let invalid_json: serde_json::Value = serde_json::from_slice(&invalid_output.stdout).unwrap();
    let invalid_item = find_config_item(&invalid_json, "pool_sweep_interval");
    assert!(invalid_item["source"].as_str().unwrap().contains("invalid"));
    assert!(invalid_item["value"].as_str().unwrap().contains("5s"));

    // invalid: parseable but zero (value-level rule, not just parseability)
    let mut cmd = Command::cargo_bin("typemux-cc").unwrap();
    cmd.arg("--doctor")
        .arg("--json")
        .env("TYPEMUX_CC_POOL_SWEEP_INTERVAL", "0");
    let zero_output = cmd.output().expect("failed to run");
    assert!(zero_output.status.success());
    let zero_json: serde_json::Value = serde_json::from_slice(&zero_output.stdout).unwrap();
    let zero_item = find_config_item(&zero_json, "pool_sweep_interval");
    assert!(zero_item["source"].as_str().unwrap().contains("invalid"));
    assert!(zero_item["value"].as_str().unwrap().contains('0'));
}

fn find_config_item<'a>(report: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    report["configuration"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["name"] == name)
        .unwrap_or_else(|| panic!("no config item named {name}"))
}
