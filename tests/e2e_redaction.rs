//! End-to-end redaction test: spawn the real binary as a subprocess and
//! prove that `main.rs` actually plumbs the PAT bytes into the
//! `RedactingWriter`. No amount of unit-testing `RedactingWriter` itself
//! covers a refactor that drops the `SecretSet::new([pat_bytes])` at the
//! call site.

use std::io::{Read, Write};
use std::process::{Command, Stdio};

/// Spawn the binary with a base_url that embeds the PAT literally, so the
/// startup banner log line contains the PAT bytes on purpose. After EOF on
/// stdin, the process should exit cleanly. Return captured stderr.
fn run_once(pat: &str) -> String {
    let base_url_with_pat = format!("https://{pat}.example.invalid");
    let init = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
"#;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_brenn-forgejo-mcp"));
    cmd.arg("--base-url")
        .arg(&base_url_with_pat)
        .env("FORGEJO_TOKEN", pat)
        .env("RUST_LOG", "info")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn binary");
    child.stdin.as_mut().unwrap().write_all(init).unwrap();
    drop(child.stdin.take());
    let _ = child.wait().expect("wait");
    let mut stderr = Vec::new();
    child.stderr.unwrap().read_to_end(&mut stderr).unwrap();
    String::from_utf8_lossy(&stderr).into_owned()
}

/// The startup banner logs `base_url=...`. Embedding the PAT in base_url
/// guarantees the banner line *would* contain the PAT verbatim if nothing
/// scrubbed it. A pass here proves `main.rs` installs the redacting writer
/// with the PAT bytes loaded.
#[test]
fn startup_banner_scrubs_pat_from_stderr() {
    let pat = "super-secret-pat-0xDEADBEEF";
    let s = run_once(pat);
    assert!(!s.contains(pat), "PAT leaked into stderr; stderr was:\n{s}");
    assert!(
        s.contains("<REDACTED>"),
        "expected <REDACTED> in stderr; stderr was:\n{s}"
    );
    assert!(
        s.contains(concat!(env!("CARGO_PKG_NAME"), " v")),
        "banner missing:\n{s}"
    );
}

/// If the TOML config file contains a `token` line and the rest of the
/// TOML is malformed, `toml::de::Error`'s `Display` includes a snippet
/// around the offending line — which can straddle the `token = "..."`
/// line and echo the PAT plaintext. The pre-logger error path must scrub
/// the PAT bytes anyway. This exercises `peek_toml_token` in main.rs.
#[cfg(unix)]
#[test]
fn malformed_toml_error_scrubs_pat_from_stderr() {
    use std::os::unix::fs::PermissionsExt;

    let pat = "super-secret-pat-in-toml-0xBADF00D";
    // Write a TOML file with the token on one line and a deliberately bad
    // line after it. `toml::de::Error` will cite the bad line; depending
    // on the toml crate's error formatting it may or may not include
    // adjacent lines, but the redactor should scrub the PAT either way.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("forgejo.toml");
    std::fs::write(
        &path,
        format!(
            "base_url = \"https://example.invalid\"\ntoken = \"{pat}\"\nbogus_field = \"oops\"\n"
        ),
    )
    .expect("write toml");
    // 0o600 so load_toml's perm check accepts it.
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(&path, perms).unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_brenn-forgejo-mcp"));
    cmd.arg("--config")
        .arg(&path)
        // No FORGEJO_TOKEN env var — the scrub set must be seeded from
        // the TOML peek only.
        .env_remove("FORGEJO_TOKEN")
        .env("RUST_LOG", "info")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn");
    drop(child.stdin.take());
    let status = child.wait().expect("wait");
    let mut stderr_bytes = Vec::new();
    child
        .stderr
        .unwrap()
        .read_to_end(&mut stderr_bytes)
        .unwrap();
    let s = String::from_utf8_lossy(&stderr_bytes);
    // Binary should have exited non-zero (config file was malformed).
    assert!(!status.success(), "expected non-zero exit; stderr:\n{s}");
    // Critical invariant: even though the TOML token is in a malformed
    // file, the PAT must not appear in the error message.
    assert!(
        !s.contains(pat),
        "TOML token leaked into pre-logger stderr; stderr was:\n{s}"
    );
}
