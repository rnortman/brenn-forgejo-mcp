//! Configuration: TOML file + env vars + CLI flags.
//!
//! Per-field precedence:
//!
//! - `token`  — exactly one of TOML `token` or env `FORGEJO_TOKEN`. Both or
//!   neither → error.
//! - `base_url` — `--base-url` CLI overrides TOML `base_url`.
//! - `timeout_secs` — TOML, default 30.
//! - `user_agent`  — TOML, default `brenn-forgejo-mcp/<version>`.
//! - `log_level`   — env `RUST_LOG` wins; else TOML `log_level`; else
//!   default `info`. `main` promotes the resolved value into `RUST_LOG`
//!   before `env_logger::init()`.

use std::fs;
use std::path::{Path, PathBuf};

use secrecy::SecretString;
use serde::Deserialize;

/// Default HTTP timeout when not set in config.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Default log level when neither `RUST_LOG` nor TOML `log_level` is set.
pub const DEFAULT_LOG_LEVEL: &str = "info";

/// Parsed TOML config. Unknown fields are rejected.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct TomlConfig {
    base_url: Option<String>,
    token: Option<String>,
    timeout_secs: Option<u64>,
    user_agent: Option<String>,
    log_level: Option<String>,
}

/// Fully resolved runtime config after merging TOML + env + CLI.
///
/// `token` is a [`SecretString`]: `Debug` and `Display` print `[REDACTED]`.
/// The plaintext is only reachable via `ExposeSecret::expose_secret()`.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub base_url: String,
    pub token: SecretString,
    pub timeout_secs: u64,
    pub user_agent: String,
    /// Effective log-level string. `main` reads this and — iff `RUST_LOG`
    /// is unset — writes it into the env so `env_logger` picks it up.
    /// `None` means "no log_level was configured; use env_logger's
    /// default_filter". `Some(s)` is the value to promote into `RUST_LOG`.
    pub log_level: Option<String>,
}

/// Errors from config resolution. These always cause the binary to exit
/// non-zero at startup; this enum exists so tests can match specific cases.
#[derive(Debug)]
pub enum ConfigError {
    /// Config file path supplied but not readable.
    FileRead(PathBuf, std::io::Error),
    /// Config file parsed but rejected (syntax, unknown field, bad type).
    FileParse(PathBuf, toml::de::Error),
    /// `base_url` not provided by any source.
    MissingBaseUrl,
    /// `token` not provided by any source.
    MissingToken,
    /// `token` set in both TOML and env var — operator ambiguity bug.
    TokenInBothTomlAndEnv,
    /// Config file permissions are too permissive (Unix: group/other
    /// readable).
    #[cfg(unix)]
    FilePermissionsTooOpen(PathBuf, u32),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::FileRead(path, e) => {
                write!(f, "config file error: reading {}: {e}", path.display())
            }
            ConfigError::FileParse(path, e) => {
                write!(f, "config file error: parsing {}: {e}", path.display())
            }
            ConfigError::MissingBaseUrl => write!(
                f,
                "missing required config: `base_url` (set it in the TOML config file or pass --base-url)"
            ),
            ConfigError::MissingToken => write!(
                f,
                "missing required config: `token` (set it in the TOML config file or via the FORGEJO_TOKEN env var)"
            ),
            ConfigError::TokenInBothTomlAndEnv => write!(
                f,
                "ambiguous config: `token` is set in both the TOML file and the FORGEJO_TOKEN env var; pick exactly one"
            ),
            #[cfg(unix)]
            ConfigError::FilePermissionsTooOpen(path, mode) => write!(
                f,
                "config file {} has permissions {:#o} (group/other bits set); expected 0o600 or stricter — token file must not be world-readable",
                path.display(),
                mode
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Load TOML config from `path` if supplied, then merge with env + CLI.
///
/// `env_token` is the current value of the `FORGEJO_TOKEN` environment
/// variable, or `None` if unset. The production caller reads this in
/// `main.rs`; tests construct it directly.
pub fn resolve(
    config_path: Option<&Path>,
    cli_base_url: Option<&str>,
    env_token: Option<String>,
) -> Result<ResolvedConfig, ConfigError> {
    let toml_cfg = match config_path {
        Some(p) => load_toml(p)?,
        None => TomlConfig::default(),
    };

    let base_url = cli_base_url
        .map(str::to_string)
        .or(toml_cfg.base_url)
        .map(|s| normalize_base_url(&s))
        .ok_or(ConfigError::MissingBaseUrl)?;

    let token: SecretString = match (toml_cfg.token, env_token) {
        (Some(_), Some(_)) => return Err(ConfigError::TokenInBothTomlAndEnv),
        (Some(t), None) | (None, Some(t)) => t.into(),
        (None, None) => return Err(ConfigError::MissingToken),
    };

    let timeout_secs = toml_cfg.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
    let user_agent = toml_cfg.user_agent.unwrap_or_else(default_user_agent);

    Ok(ResolvedConfig {
        base_url,
        token,
        timeout_secs,
        user_agent,
        log_level: toml_cfg.log_level,
    })
}

fn load_toml(path: &Path) -> Result<TomlConfig, ConfigError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = fs::metadata(path).map_err(|e| ConfigError::FileRead(path.to_path_buf(), e))?;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(ConfigError::FilePermissionsTooOpen(
                path.to_path_buf(),
                mode,
            ));
        }
    }
    let text =
        fs::read_to_string(path).map_err(|e| ConfigError::FileRead(path.to_path_buf(), e))?;
    toml::from_str(&text).map_err(|e| ConfigError::FileParse(path.to_path_buf(), e))
}

/// Strip a trailing `/` from `base_url` so URL joins don't produce `//`.
fn normalize_base_url(s: &str) -> String {
    s.trim_end_matches('/').to_string()
}

/// Default `User-Agent` header: `brenn-forgejo-mcp/<CARGO_PKG_VERSION>`.
fn default_user_agent() -> String {
    format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
}

/// Best-effort: return the TOML `token` plaintext if it can be extracted
/// from the file at `path`. Used only for pre-logger scrub-set seeding,
/// so this intentionally swallows I/O and parse errors — the caller is
/// about to surface a separate error from `resolve()`. If the TOML parses
/// cleanly, we read the typed `token`; if it doesn't (which is the whole
/// reason the scrub-set is needed), we fall back to a lenient line scan
/// for `token = "..."`.
///
/// Returns `None` if there's no config path, the file can't be read, no
/// `token` field is present, or the token value is empty.
pub fn peek_toml_token(path: Option<&Path>) -> Option<String> {
    let p = path?;
    let text = fs::read_to_string(p).ok()?;
    if let Ok(cfg) = toml::from_str::<TomlConfig>(&text)
        && let Some(t) = cfg.token
        && !t.is_empty()
    {
        return Some(t);
    }
    // Lenient line scan — the TOML didn't parse (that's precisely when we
    // care most about scrubbing the operator's error), so hunt for
    //   token = "..."
    // by hand. We don't handle multi-line strings or escapes; those are
    // unusual for a PAT and the scrub-set is best-effort anyway.
    for line in text.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("token") else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('"') else {
            continue;
        };
        if let Some(end) = rest.find('"')
            && end > 0
        {
            return Some(rest[..end].to_string());
        }
    }
    None
}

/// Promote a configured log level into the `RUST_LOG` environment variable,
/// but only if `RUST_LOG` is not already set — the operator always wins.
/// If `toml_log_level` is `None`, `DEFAULT_LOG_LEVEL` is used.
///
/// This is in its own function rather than inlined in `main.rs` so it can
/// be unit-tested under `#[serial_test::serial]`.
///
/// # Safety
///
/// The caller must guarantee single-threaded execution — at the moment of
/// the call, no other thread must be reading `RUST_LOG`. In practice this
/// is called in `main()` before `env_logger::init()` and before any worker
/// threads are spawned. The tests use `#[serial_test::serial]` to enforce
/// this manually across the env-mutating tests.
pub unsafe fn promote_log_level(toml_log_level: Option<&str>) {
    if std::env::var_os("RUST_LOG").is_some() {
        return;
    }
    let level = toml_log_level.unwrap_or(DEFAULT_LOG_LEVEL);
    debug_assert!(std::env::var_os("RUST_LOG").is_none());
    // SAFETY: delegated to caller — see function doc.
    unsafe {
        std::env::set_var("RUST_LOG", level);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;
    use std::io::Write;

    /// Build a temp TOML file with 0o600 permissions so `load_toml`'s
    /// permission check doesn't reject it during tests. On non-Unix the
    /// check is compiled out.
    fn write_toml(contents: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(f.path()).unwrap().permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(f.path(), perms).unwrap();
        }
        f
    }

    #[test]
    fn resolve_env_only_token_ok() {
        let cfg = resolve(None, Some("https://example.com"), Some("tok".into())).unwrap();
        assert_eq!(cfg.base_url, "https://example.com");
        assert_eq!(cfg.token.expose_secret(), "tok");
        assert_eq!(cfg.timeout_secs, DEFAULT_TIMEOUT_SECS);
    }

    #[test]
    fn resolve_missing_base_url_errors() {
        let err = resolve(None, None, Some("tok".into())).unwrap_err();
        assert!(matches!(err, ConfigError::MissingBaseUrl));
    }

    #[test]
    fn resolve_missing_token_errors() {
        let err = resolve(None, Some("https://example.com"), None).unwrap_err();
        assert!(matches!(err, ConfigError::MissingToken));
    }

    #[test]
    fn resolve_token_in_both_errors() {
        let f = write_toml("base_url = \"https://x\"\ntoken = \"from_toml\"\n");
        let err = resolve(Some(f.path()), None, Some("from_env".into())).unwrap_err();
        assert!(matches!(err, ConfigError::TokenInBothTomlAndEnv));
    }

    #[test]
    fn resolve_loads_toml() {
        let f = write_toml(
            "base_url = \"https://example.com\"\ntoken = \"tok\"\ntimeout_secs = 60\nuser_agent = \"x/1\"\n",
        );
        let cfg = resolve(Some(f.path()), None, None).unwrap();
        assert_eq!(cfg.base_url, "https://example.com");
        assert_eq!(cfg.token.expose_secret(), "tok");
        assert_eq!(cfg.timeout_secs, 60);
        assert_eq!(cfg.user_agent, "x/1");
    }

    #[test]
    fn resolve_defaults_user_agent_to_package_name_and_version() {
        let f = write_toml("base_url = \"https://example.com\"\ntoken = \"tok\"\n");
        let cfg = resolve(Some(f.path()), None, None).unwrap();
        assert_eq!(
            cfg.user_agent,
            concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn resolve_cli_base_url_overrides_toml() {
        let f = write_toml("base_url = \"https://from-toml\"\ntoken = \"tok\"\n");
        let cfg = resolve(Some(f.path()), Some("https://from-cli"), None).unwrap();
        assert_eq!(cfg.base_url, "https://from-cli");
    }

    #[test]
    fn resolve_rejects_unknown_toml_field() {
        let f = write_toml("base_url = \"https://x\"\ntoken = \"t\"\nbogus = \"field\"\n");
        let err = resolve(Some(f.path()), None, None).unwrap_err();
        assert!(matches!(err, ConfigError::FileParse(..)));
    }

    #[test]
    fn resolve_distinguishes_read_from_parse_errors() {
        // Missing file → FileRead.
        let missing = std::path::PathBuf::from("/nonexistent/path/forgejo.toml");
        let err = resolve(Some(&missing), None, None).unwrap_err();
        assert!(matches!(err, ConfigError::FileRead(..)), "got: {err}");
    }

    #[test]
    fn base_url_trailing_slash_stripped() {
        let cfg = resolve(None, Some("https://example.com/"), Some("t".into())).unwrap();
        assert_eq!(cfg.base_url, "https://example.com");
    }

    #[test]
    fn base_url_no_trailing_slash_preserved() {
        let cfg = resolve(None, Some("https://example.com"), Some("t".into())).unwrap();
        assert_eq!(cfg.base_url, "https://example.com");
    }

    /// `{:?}` on `ResolvedConfig` must not leak the plaintext token.
    #[test]
    fn resolved_config_debug_redacts_token() {
        let cfg = resolve(
            None,
            Some("https://example.com"),
            Some("super-secret-pat-1234".into()),
        )
        .unwrap();
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains("super-secret-pat-1234"), "debug was: {dbg}");
    }

    #[test]
    fn resolve_loads_log_level_from_toml() {
        let f = write_toml("base_url = \"https://x\"\ntoken = \"t\"\nlog_level = \"debug\"\n");
        let cfg = resolve(Some(f.path()), None, None).unwrap();
        assert_eq!(cfg.log_level.as_deref(), Some("debug"));
    }

    #[test]
    fn resolve_log_level_absent_is_none() {
        let cfg = resolve(None, Some("https://x"), Some("t".into())).unwrap();
        assert!(cfg.log_level.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn resolve_rejects_world_readable_config() {
        use std::os::unix::fs::PermissionsExt;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"base_url = \"https://x\"\ntoken = \"t\"\n")
            .unwrap();
        let mut perms = std::fs::metadata(f.path()).unwrap().permissions();
        perms.set_mode(0o644); // group/other readable → reject
        std::fs::set_permissions(f.path(), perms).unwrap();
        let err = resolve(Some(f.path()), None, None).unwrap_err();
        assert!(
            matches!(err, ConfigError::FilePermissionsTooOpen(..)),
            "got: {err}"
        );
    }

    // ---------------------------------------------------------------------
    // promote_log_level — env var manipulation, serialized.
    //
    // `std::env::set_var` / `remove_var` are `unsafe` in edition 2024
    // because concurrent env mutation is UB. We serialize these tests with
    // `serial_test::serial` so cargo test's default thread pool can't race
    // them against each other. Inside each test we're the only writer.
    // ---------------------------------------------------------------------

    /// Capture the current `RUST_LOG` value and restore it on drop, so
    /// these tests leave the environment untouched for adjacent tests.
    struct RustLogGuard(Option<std::ffi::OsString>);
    impl RustLogGuard {
        fn snapshot() -> Self {
            RustLogGuard(std::env::var_os("RUST_LOG"))
        }
    }
    impl Drop for RustLogGuard {
        fn drop(&mut self) {
            // SAFETY: tests calling this are #[serial]; no other thread
            // is reading RUST_LOG right now.
            unsafe {
                match self.0.take() {
                    Some(v) => std::env::set_var("RUST_LOG", v),
                    None => std::env::remove_var("RUST_LOG"),
                }
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn promote_log_level_no_op_if_rust_log_is_already_set() {
        let _guard = RustLogGuard::snapshot();
        // SAFETY: serial test, exclusive writer.
        unsafe {
            std::env::set_var("RUST_LOG", "operator_chose_this");
            promote_log_level(Some("ignored_because_env_wins"));
        }
        assert_eq!(std::env::var("RUST_LOG").unwrap(), "operator_chose_this");
    }

    #[test]
    #[serial_test::serial]
    fn promote_log_level_writes_toml_value_when_env_unset() {
        let _guard = RustLogGuard::snapshot();
        // SAFETY: serial test.
        unsafe {
            std::env::remove_var("RUST_LOG");
            promote_log_level(Some("debug"));
        }
        assert_eq!(std::env::var("RUST_LOG").unwrap(), "debug");
    }

    #[test]
    #[serial_test::serial]
    fn promote_log_level_writes_default_when_env_and_toml_unset() {
        let _guard = RustLogGuard::snapshot();
        // SAFETY: serial test.
        unsafe {
            std::env::remove_var("RUST_LOG");
            promote_log_level(None);
        }
        assert_eq!(std::env::var("RUST_LOG").unwrap(), DEFAULT_LOG_LEVEL);
    }

    // ---------------------------------------------------------------------
    // peek_toml_token — best-effort TOML token extraction for pre-logger
    // scrub-set seeding.
    // ---------------------------------------------------------------------

    #[test]
    fn peek_toml_token_returns_none_when_no_path() {
        assert!(peek_toml_token(None).is_none());
    }

    #[test]
    fn peek_toml_token_returns_none_when_file_missing() {
        let missing = std::path::PathBuf::from("/nonexistent/peek-test.toml");
        assert!(peek_toml_token(Some(&missing)).is_none());
    }

    #[test]
    fn peek_toml_token_extracts_via_typed_parse_on_clean_toml() {
        let f = write_toml("base_url = \"https://x\"\ntoken = \"peek-me\"\n");
        assert_eq!(peek_toml_token(Some(f.path())).as_deref(), Some("peek-me"));
    }

    #[test]
    fn peek_toml_token_extracts_via_line_scan_on_malformed_toml() {
        // Unknown field triggers deny_unknown_fields → parse fails; the
        // lenient line scan should still find the token. This is the case
        // NEW-2 cares about: malformed TOML with the token line still
        // readable.
        let f = write_toml(
            "base_url = \"https://x\"\ntoken = \"line-scan-me\"\nbogus_field = \"oops\"\n",
        );
        // Confirm the typed parse does fail (sanity check).
        assert!(resolve(Some(f.path()), None, None).is_err());
        assert_eq!(
            peek_toml_token(Some(f.path())).as_deref(),
            Some("line-scan-me")
        );
    }

    #[test]
    fn peek_toml_token_line_scan_handles_whitespace_variations() {
        let f =
            write_toml("base_url = \"https://x\"\n  token  =   \"whitespace-ok\"  \nbogus = 1\n");
        assert_eq!(
            peek_toml_token(Some(f.path())).as_deref(),
            Some("whitespace-ok")
        );
    }

    #[test]
    fn peek_toml_token_returns_none_on_empty_token_value() {
        let f = write_toml("base_url = \"https://x\"\ntoken = \"\"\nbogus = 1\n");
        assert!(peek_toml_token(Some(f.path())).is_none());
    }

    #[test]
    fn peek_toml_token_ignores_unrelated_token_word_in_other_fields() {
        // `user_agent` starts with nothing related; "token" only appears as
        // part of a sub-key or comment-like string. Line-scan must not
        // match `some_token = "..."` because that starts with a different
        // word.
        let f = write_toml(
            "base_url = \"https://x\"\nuser_agent = \"has the word token in it\"\nbogus = 1\n",
        );
        assert!(peek_toml_token(Some(f.path())).is_none());
    }
}
