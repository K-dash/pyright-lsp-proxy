use crate::backend::BackendKind;
use crate::backend_pool;
use crate::config::ConfigLoadReport;
use crate::venv;
use clap::parser::ValueSource;
use clap::ArgMatches;
use serde::Serialize;
use std::fmt::Write as _;
use std::path::PathBuf;

/// Top-level doctor report.
#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub version: String,
    pub config_file: ConfigFileReport,
    pub configuration: ConfigReport,
    pub environment: EnvironmentReport,
    pub system: SystemReport,
}

#[derive(Debug, Serialize)]
pub struct ConfigFileReport {
    pub path: String,
    pub exists: bool,
    pub loaded_keys: Vec<String>,
    pub skipped_keys: Vec<String>,
    pub parse_errors: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ConfigReport {
    pub items: Vec<ConfigItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigItem {
    pub name: String,
    pub value: String,
    pub source: String,
}

#[derive(Debug, Serialize)]
pub struct EnvironmentReport {
    pub backend_binary: BackendBinaryInfo,
    pub git_toplevel: Option<String>,
    pub fallback_venv: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BackendBinaryInfo {
    pub command: String,
    pub path: Option<String>,
    pub version: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SystemReport {
    pub os: String,
    pub arch: String,
}

/// Determine the source label for a CLI argument, with config-file provenance.
fn arg_source(matches: &ArgMatches, id: &str, config_report: &ConfigLoadReport) -> String {
    match matches.value_source(id) {
        Some(ValueSource::CommandLine) => "cli".to_string(),
        Some(ValueSource::EnvVariable) => {
            let env_name = env_var_name(id);
            // If this env var was loaded from config file, report as config source
            if config_report.loaded_keys.contains(&env_name.to_string()) {
                format!("config: {}", config_report.path.display())
            } else {
                format!("env: {}", env_name)
            }
        }
        _ => "default".to_string(),
    }
}

/// Map arg id to its environment variable name.
fn env_var_name(id: &str) -> &'static str {
    match id {
        "backend" => "TYPEMUX_CC_BACKEND",
        "max_backends" => "TYPEMUX_CC_MAX_BACKENDS",
        "backend_ttl" => "TYPEMUX_CC_BACKEND_TTL",
        "log_file" => "TYPEMUX_CC_LOG_FILE",
        _ => "UNKNOWN",
    }
}

/// Determine source for env-var-only settings (no CLI arg), with config-file provenance.
fn env_only_source(env_var: &str, config_report: &ConfigLoadReport) -> String {
    if std::env::var(env_var).is_ok() {
        if config_report.loaded_keys.contains(&env_var.to_string()) {
            format!("config: {}", config_report.path.display())
        } else {
            format!("env: {}", env_var)
        }
    } else {
        "default".to_string()
    }
}

/// Search for a binary in PATH directories using `std::env::split_paths` + metadata.
/// Returns the first match that is a file with execute permission.
pub fn find_binary_in_path(binary_name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(binary_name);
        if let Ok(meta) = std::fs::metadata(&candidate) {
            if meta.is_file() && is_executable(&meta) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    // On non-Unix platforms, assume files in PATH are executable.
    true
}

/// Run `<command> --version` and return stdout, or an error description.
async fn detect_backend_version(command: &str) -> Option<String> {
    let child = match tokio::process::Command::new(command)
        .arg("--version")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return Some(format!("failed to run: {}", e)),
    };

    match child.wait_with_output().await {
        Ok(out) if out.status.success() => {
            let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if version.is_empty() {
                // Some tools print version to stderr
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                if stderr.is_empty() {
                    Some("(empty output)".to_string())
                } else {
                    Some(stderr)
                }
            } else {
                Some(version)
            }
        }
        Ok(out) => Some(format!(
            "exit code {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(e) => Some(format!("failed to read output: {}", e)),
    }
}

/// Collect all diagnostic information into a `DoctorReport`.
pub async fn collect_doctor_report(
    backend: &BackendKind,
    matches: &ArgMatches,
    config_report: &ConfigLoadReport,
) -> DoctorReport {
    let version = env!("CARGO_PKG_VERSION").to_string();

    // Config file report
    let config_file = ConfigFileReport {
        path: config_report.path.display().to_string(),
        exists: config_report.exists,
        loaded_keys: config_report.loaded_keys.clone(),
        skipped_keys: config_report.skipped_keys.clone(),
        parse_errors: config_report
            .parse_errors
            .iter()
            .map(|e| format!("line {}: {} ({})", e.line_number, e.reason, e.raw_line))
            .collect(),
    };

    // Configuration items from clap ArgMatches
    let backend_item = ConfigItem {
        name: "backend".to_string(),
        value: backend.display_name().to_string(),
        source: arg_source(matches, "backend", config_report),
    };

    let max_backends_value: String = matches
        .get_one::<u64>("max_backends")
        .map_or_else(|| "8".to_string(), std::string::ToString::to_string);
    let max_backends_item = ConfigItem {
        name: "max_backends".to_string(),
        value: max_backends_value,
        source: arg_source(matches, "max_backends", config_report),
    };

    let backend_ttl_value: String = matches
        .get_one::<u64>("backend_ttl")
        .map_or_else(|| "1800".to_string(), std::string::ToString::to_string);
    let backend_ttl_item = ConfigItem {
        name: "backend_ttl".to_string(),
        value: backend_ttl_value,
        source: arg_source(matches, "backend_ttl", config_report),
    };

    let warmup_timeout = backend_pool::warmup_timeout();
    let warmup_timeout_item = ConfigItem {
        name: "warmup_timeout".to_string(),
        value: warmup_timeout.as_secs().to_string(),
        source: env_only_source("TYPEMUX_CC_WARMUP_TIMEOUT", config_report),
    };

    let fanout_timeout = backend_pool::fanout_timeout();
    let fanout_timeout_item = ConfigItem {
        name: "fanout_timeout".to_string(),
        value: fanout_timeout.as_secs().to_string(),
        source: env_only_source("TYPEMUX_CC_FANOUT_TIMEOUT", config_report),
    };

    let log_file_value: String = matches
        .get_one::<PathBuf>("log_file")
        .map_or_else(|| "<not set>".to_string(), |p| p.display().to_string());
    let log_file_source = if matches.value_source("log_file") == Some(ValueSource::DefaultValue)
        || matches.value_source("log_file").is_none()
    {
        "default".to_string()
    } else {
        arg_source(matches, "log_file", config_report)
    };
    let log_file_item = ConfigItem {
        name: "log_file".to_string(),
        value: log_file_value,
        source: log_file_source,
    };

    let config = ConfigReport {
        items: vec![
            backend_item,
            max_backends_item,
            backend_ttl_item,
            warmup_timeout_item,
            fanout_timeout_item,
            log_file_item,
        ],
    };

    // Environment: backend binary
    let cmd_name = backend.command();
    let binary_path = find_binary_in_path(cmd_name);
    let version_cmd = backend.version_command();
    let backend_version = detect_backend_version(version_cmd).await;
    let backend_binary = BackendBinaryInfo {
        command: cmd_name.to_string(),
        path: binary_path.map(|p| p.display().to_string()),
        version: backend_version,
    };

    // Environment: git toplevel and fallback venv
    let cwd = std::env::current_dir().unwrap_or_default();
    let git_toplevel = venv::get_git_toplevel(&cwd).await.ok().flatten();
    let fallback_venv = venv::find_fallback_venv(&cwd).await.ok().flatten();

    let environment = EnvironmentReport {
        backend_binary,
        git_toplevel: git_toplevel.map(|p| p.display().to_string()),
        fallback_venv: fallback_venv.map(|p| p.display().to_string()),
    };

    // System info
    let system = SystemReport {
        os: format!("{} ({})", std::env::consts::OS, os_version()),
        arch: std::env::consts::ARCH.to_string(),
    };

    DoctorReport {
        version,
        config_file,
        configuration: config,
        environment,
        system,
    }
}

/// Get OS kernel version string.
fn os_version() -> String {
    #[cfg(unix)]
    {
        let mut utsname = std::mem::MaybeUninit::<libc::utsname>::uninit();
        // SAFETY: `utsname.as_mut_ptr()` points to valid, writable memory sized
        // for `libc::utsname`; `uname` only writes into it.
        #[allow(unsafe_code)]
        let ret = unsafe { libc::uname(utsname.as_mut_ptr()) };
        if ret == 0 {
            // SAFETY: `ret == 0` confirms `uname` fully initialized `utsname`.
            #[allow(unsafe_code)]
            let utsname = unsafe { utsname.assume_init() };
            // SAFETY: `sysname`/`release` are nul-terminated C strings written
            // by the successful `uname` call above, valid for `utsname`'s scope.
            #[allow(unsafe_code)]
            let sysname = unsafe { std::ffi::CStr::from_ptr(utsname.sysname.as_ptr()) };
            #[allow(unsafe_code)]
            let release = unsafe { std::ffi::CStr::from_ptr(utsname.release.as_ptr()) };
            return format!(
                "{} {}",
                sysname.to_string_lossy(),
                release.to_string_lossy()
            );
        }
    }
    "unknown".to_string()
}

/// Render the report in human-readable aligned columns.
pub fn render_human(report: &DoctorReport) -> String {
    let mut out = String::new();

    let _ = writeln!(out, "typemux-cc v{}", report.version);

    // Config file info
    out.push_str("\nConfig file:\n");
    let _ = writeln!(out, "  Path              {}", report.config_file.path);
    let _ = writeln!(
        out,
        "  Status            {}",
        if report.config_file.exists {
            "loaded"
        } else {
            "not found"
        }
    );
    if !report.config_file.parse_errors.is_empty() {
        out.push_str("  Warnings:\n");
        for err in &report.config_file.parse_errors {
            let _ = writeln!(out, "    ⚠ {err}");
        }
    }

    out.push_str("\nConfiguration:\n");

    // Find max key and value widths for alignment
    let max_key = report
        .configuration
        .items
        .iter()
        .map(|i| i.name.len())
        .max()
        .unwrap_or(0);
    let max_val = report
        .configuration
        .items
        .iter()
        .map(|i| i.value.len())
        .max()
        .unwrap_or(0);

    for item in &report.configuration.items {
        let _ = writeln!(
            out,
            "  {:<max_key$}  {:<max_val$}  ({})",
            item.name, item.value, item.source,
        );
    }

    out.push_str("\nEnvironment:\n");
    let _ = writeln!(
        out,
        "  Backend binary    {}",
        report.environment.backend_binary.command
    );
    let _ = writeln!(
        out,
        "    Path            {}",
        report
            .environment
            .backend_binary
            .path
            .as_deref()
            .unwrap_or("<not found>")
    );
    let _ = writeln!(
        out,
        "    Version         {}",
        report
            .environment
            .backend_binary
            .version
            .as_deref()
            .unwrap_or("<unknown>")
    );
    let _ = writeln!(
        out,
        "  Git toplevel      {}",
        report
            .environment
            .git_toplevel
            .as_deref()
            .unwrap_or("<not in a git repo>")
    );
    let _ = writeln!(
        out,
        "  Fallback venv     {}",
        report
            .environment
            .fallback_venv
            .as_deref()
            .unwrap_or("<not found>")
    );

    out.push_str("\nSystem:\n");
    let _ = writeln!(out, "  OS                {}", report.system.os);
    let _ = writeln!(out, "  Arch              {}", report.system.arch);

    out
}

/// Entry point: collect report, render, and print to stdout. Always exits 0.
pub async fn run_doctor(
    backend: &BackendKind,
    json: bool,
    matches: &ArgMatches,
    config_report: &ConfigLoadReport,
) {
    let report = collect_doctor_report(backend, matches, config_report).await;

    if json {
        match serde_json::to_string_pretty(&report) {
            Ok(json_str) => print!("{}", json_str),
            Err(e) => eprintln!("Failed to serialize doctor report: {}", e),
        }
    } else {
        print!("{}", render_human(&report));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_binary_in_path_finds_sh() {
        // /bin/sh should exist on all Unix systems
        let result = find_binary_in_path("sh");
        assert!(result.is_some(), "sh should be found in PATH");
    }

    #[test]
    fn find_binary_in_path_returns_none_for_nonexistent() {
        let result = find_binary_in_path("this-binary-does-not-exist-12345");
        assert!(result.is_none());
    }

    #[test]
    fn render_human_output_structure() {
        let report = DoctorReport {
            version: "0.2.8".to_string(),
            config_file: ConfigFileReport {
                path: "/home/test/.config/typemux-cc/config".to_string(),
                exists: true,
                loaded_keys: vec![],
                skipped_keys: vec![],
                parse_errors: vec![],
            },
            configuration: ConfigReport {
                items: vec![
                    ConfigItem {
                        name: "backend".to_string(),
                        value: "pyright".to_string(),
                        source: "default".to_string(),
                    },
                    ConfigItem {
                        name: "max_backends".to_string(),
                        value: "8".to_string(),
                        source: "default".to_string(),
                    },
                ],
            },
            environment: EnvironmentReport {
                backend_binary: BackendBinaryInfo {
                    command: "pyright-langserver".to_string(),
                    path: Some("/usr/local/bin/pyright-langserver".to_string()),
                    version: Some("pyright 1.1.350".to_string()),
                },
                git_toplevel: Some("/Users/test/project".to_string()),
                fallback_venv: Some("/Users/test/project/.venv".to_string()),
            },
            system: SystemReport {
                os: "macos (Darwin 24.0.0)".to_string(),
                arch: "aarch64".to_string(),
            },
        };

        let output = render_human(&report);

        assert!(output.starts_with("typemux-cc v0.2.8\n"));
        assert!(output.contains("Config file:"));
        assert!(output.contains("loaded"));
        assert!(output.contains("Configuration:"));
        assert!(output.contains("backend"));
        assert!(output.contains("pyright"));
        assert!(output.contains("(default)"));
        assert!(output.contains("Environment:"));
        assert!(output.contains("Backend binary"));
        assert!(output.contains("pyright-langserver"));
        assert!(output.contains("System:"));
        assert!(output.contains("aarch64"));
    }

    #[test]
    fn render_human_handles_missing_values() {
        let report = DoctorReport {
            version: "0.2.8".to_string(),
            config_file: ConfigFileReport {
                path: "/home/test/.config/typemux-cc/config".to_string(),
                exists: false,
                loaded_keys: vec![],
                skipped_keys: vec![],
                parse_errors: vec![],
            },
            configuration: ConfigReport { items: vec![] },
            environment: EnvironmentReport {
                backend_binary: BackendBinaryInfo {
                    command: "pyright-langserver".to_string(),
                    path: None,
                    version: None,
                },
                git_toplevel: None,
                fallback_venv: None,
            },
            system: SystemReport {
                os: "linux".to_string(),
                arch: "x86_64".to_string(),
            },
        };

        let output = render_human(&report);
        assert!(output.contains("<not found>"));
        assert!(output.contains("<unknown>"));
        assert!(output.contains("<not in a git repo>"));
    }

    #[test]
    fn config_item_serializes_correctly() {
        let item = ConfigItem {
            name: "backend".to_string(),
            value: "pyright".to_string(),
            source: "default".to_string(),
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("\"name\":\"backend\""));
        assert!(json.contains("\"source\":\"default\""));
    }
}
