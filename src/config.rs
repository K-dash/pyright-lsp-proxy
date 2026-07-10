use std::path::PathBuf;

/// A single parse error with location context.
#[derive(Debug, Clone)]
pub struct ParseError {
    pub line_number: usize,
    pub raw_line: String,
    pub reason: String,
}

/// Report from loading the config file, used by doctor for provenance tracking.
#[derive(Debug)]
pub struct ConfigLoadReport {
    pub path: PathBuf,
    pub exists: bool,
    pub loaded_keys: Vec<String>,
    pub skipped_keys: Vec<String>,
    pub parse_errors: Vec<ParseError>,
}

/// Default config file path: `~/.config/typemux-cc/config`
fn default_config_path() -> Option<PathBuf> {
    dirs_fallback_home().map(|home| home.join(".config").join("typemux-cc").join("config"))
}

/// Get home directory without external crates.
fn dirs_fallback_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Parse a single non-empty, non-comment line into (key, value).
///
/// Supports:
///   KEY=VALUE
///   export KEY=VALUE
///   KEY="VALUE"  /  KEY='VALUE'
///
/// Does NOT support shell expansion ($HOME, command substitution, etc.).
fn parse_line(line: &str) -> Result<(String, String), String> {
    let trimmed = line.trim();

    // Strip optional `export ` prefix
    let trimmed = trimmed
        .strip_prefix("export ")
        .unwrap_or(trimmed)
        .trim_start();

    let eq_pos = trimmed
        .find('=')
        .ok_or_else(|| "missing '=' delimiter".to_string())?;

    let key = trimmed[..eq_pos].trim();
    if key.is_empty() {
        return Err("empty key".to_string());
    }
    if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!("invalid key characters: {}", key));
    }

    let mut value = trimmed[eq_pos + 1..].trim().to_string();

    // Strip matching quotes
    if ((value.starts_with('"') && value.ends_with('"'))
        || (value.starts_with('\'') && value.ends_with('\'')))
        && value.len() >= 2
    {
        value = value[1..value.len() - 1].to_string();
    }

    // Reject shell expansion patterns
    if value.contains("$(") || value.contains('`') {
        return Err("shell command substitution is not supported".to_string());
    }
    if value.contains('$') {
        return Err("shell variable expansion is not supported".to_string());
    }

    Ok((key.to_string(), value))
}

/// Load config file and set environment variables.
///
/// - Reads `~/.config/typemux-cc/config`
/// - Sets env vars only if they are not already set (existing env takes priority)
/// - Duplicate keys within config: last wins
/// - Must be called BEFORE clap argument parsing
pub fn load_config_file() -> ConfigLoadReport {
    let path = default_config_path().unwrap_or_else(|| PathBuf::from(""));

    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(_) => {
            return ConfigLoadReport {
                exists: !path.as_os_str().is_empty() && path.exists(),
                path,
                loaded_keys: vec![],
                skipped_keys: vec![],
                parse_errors: vec![],
            };
        }
    };

    let mut loaded_keys = Vec::new();
    let mut skipped_keys = Vec::new();
    let mut parse_errors = Vec::new();

    // First pass: collect all key-value pairs (last wins for duplicates)
    let mut entries: Vec<(String, String)> = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        match parse_line(line) {
            Ok((key, value)) => {
                // Last wins: remove earlier entry for the same key
                entries.retain(|(k, _)| k != &key);
                entries.push((key, value));
            }
            Err(reason) => {
                parse_errors.push(ParseError {
                    line_number: i + 1,
                    raw_line: line.to_string(),
                    reason,
                });
            }
        }
    }

    // Second pass: set env vars (skip if already set)
    for (key, value) in entries {
        if std::env::var_os(&key).is_some() {
            skipped_keys.push(key);
        } else {
            // SAFETY: called before any threads are spawned (before tokio runtime)
            #[allow(unsafe_code)]
            unsafe {
                std::env::set_var(&key, &value);
            }
            loaded_keys.push(key);
        }
    }

    ConfigLoadReport {
        exists: true,
        path,
        loaded_keys,
        skipped_keys,
        parse_errors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_key_value() {
        let (k, v) = parse_line("FOO=bar").unwrap();
        assert_eq!(k, "FOO");
        assert_eq!(v, "bar");
    }

    #[test]
    fn parse_export_prefix() {
        let (k, v) = parse_line("export FOO=bar").unwrap();
        assert_eq!(k, "FOO");
        assert_eq!(v, "bar");
    }

    #[test]
    fn parse_quoted_value_double() {
        let (k, v) = parse_line(r#"FOO="hello world""#).unwrap();
        assert_eq!(k, "FOO");
        assert_eq!(v, "hello world");
    }

    #[test]
    fn parse_quoted_value_single() {
        let (k, v) = parse_line("FOO='hello world'").unwrap();
        assert_eq!(k, "FOO");
        assert_eq!(v, "hello world");
    }

    #[test]
    fn parse_export_with_quotes() {
        let (k, v) = parse_line(r#"export MY_VAR="/some/path""#).unwrap();
        assert_eq!(k, "MY_VAR");
        assert_eq!(v, "/some/path");
    }

    #[test]
    fn reject_shell_variable() {
        let err = parse_line("FOO=$HOME/bar").unwrap_err();
        assert!(err.contains("variable expansion"));
    }

    #[test]
    fn reject_command_substitution() {
        let err = parse_line("FOO=$(whoami)").unwrap_err();
        assert!(err.contains("command substitution"));
    }

    #[test]
    fn reject_backtick_substitution() {
        let err = parse_line("FOO=`whoami`").unwrap_err();
        assert!(err.contains("command substitution"));
    }

    #[test]
    fn reject_missing_equals() {
        let err = parse_line("FOOBAR").unwrap_err();
        assert!(err.contains("missing '='"));
    }

    #[test]
    fn reject_empty_key() {
        let err = parse_line("=value").unwrap_err();
        assert!(err.contains("empty key"));
    }

    #[test]
    fn reject_invalid_key_chars() {
        let err = parse_line("FOO-BAR=value").unwrap_err();
        assert!(err.contains("invalid key"));
    }

    #[test]
    fn parse_empty_value() {
        let (k, v) = parse_line("FOO=").unwrap();
        assert_eq!(k, "FOO");
        assert_eq!(v, "");
    }

    #[test]
    fn parse_value_with_equals() {
        let (k, v) = parse_line("FOO=bar=baz").unwrap();
        assert_eq!(k, "FOO");
        assert_eq!(v, "bar=baz");
    }
}
