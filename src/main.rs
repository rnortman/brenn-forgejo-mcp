//! Entry point. Loads config, installs the redacting logger and panic
//! hook, runs the MCP server.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use brenn_mcp::Server;
use clap::Parser;
use log::{LevelFilter, info};

use brenn_forgejo_mcp::client::ForgejoClient;
use brenn_forgejo_mcp::config::{ResolvedConfig, peek_toml_token, promote_log_level, resolve};
use brenn_forgejo_mcp::redact::{RedactingWriter, SecretSet};
use brenn_forgejo_mcp::tools::{
    ForgejoAddReviewCommentTool, ForgejoClosePrTool, ForgejoGetReviewCommentsTool,
    ForgejoOpenPrTool, ForgejoPostReviewTool,
};

const INSTRUCTIONS: &str = "\
This server exposes Forgejo pull-request operations: open a PR, read existing reviews and inline comments, post a review (approve / request changes / comment, optionally with inline comments), and close a PR. PR numbers are per-repo (\"index\" arg).

Inline-comment field naming is NOT symmetric between read and write (this reflects Forgejo's own API, we don't translate):
- When you READ comments via `forgejo_get_review_comments`, each comment has `position` (line on the post-change side) and `original_position` (line on the pre-change side).
- When you WRITE comments via `forgejo_post_review`, use `new_position` (post-change) and `old_position` (pre-change), and provide exactly one of the two per comment.

Errors returned as `isError: true` are safe to show the user. Transient errors (prefix \"Forgejo transient error:\") may be retried; API errors (prefix \"Forgejo API error (XXX):\") indicate an input or auth problem and should not be retried without changing something.

`forgejo_add_review_comment` appends an inline comment to an existing review (identified by `review_id`). Forgejo's REST API does not thread replies under a specific parent comment, but the UI groups comments on the same file+line into one conversation, so posting at the same position as a reviewer's comment reads as a reply.\
";

#[derive(Parser)]
#[command(version, about = "Forgejo pull-request tools as an MCP server (stdio)")]
struct Cli {
    /// Path to the TOML config file.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Override TOML `base_url`.
    #[arg(long)]
    base_url: Option<String>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    // Build a pre-logger scrub set covering BOTH possible token sources so
    // we can scrub a pre-logger `eprintln!` when `resolve()` fails. The
    // env var is read once here and threaded through; the TOML token is
    // best-effort peeked (swallows I/O and parse errors, since resolve()
    // is about to surface the real one). See NEW-2 rationale.
    let env_token = std::env::var("FORGEJO_TOKEN").ok();
    let pre_logger_secrets = {
        let mut b = SecretSet::builder();
        if let Some(t) = env_token.as_deref()
            && !t.is_empty()
        {
            b = b.push_bytes(t.as_bytes().to_vec());
        }
        if let Some(t) = peek_toml_token(cli.config.as_deref())
            && !t.is_empty()
        {
            b = b.push_bytes(t.into_bytes());
        }
        b.build()
    };

    let cfg = match resolve(
        cli.config.as_deref(),
        cli.base_url.as_deref(),
        env_token.clone(),
    ) {
        Ok(c) => c,
        Err(e) => {
            // Logger isn't up yet. The `ConfigError::Display` impl doesn't
            // format the token value directly, but a `toml::de::Error` can
            // include line/column snippets that straddle the
            // `token = "..."` line. Scrub against the pre-logger set which
            // covers both env and TOML sources.
            let msg = format!("{} config error: {e}", env!("CARGO_PKG_NAME"));
            let scrubbed =
                brenn_forgejo_mcp::redact::scrub_bytes(msg.as_bytes(), &pre_logger_secrets);
            let _ = std::io::stderr().write_all(&scrubbed);
            let _ = std::io::stderr().write_all(b"\n");
            return ExitCode::from(2);
        }
    };
    // Pre-logger scrub set is no longer needed — the real one below
    // supersedes it (and holds the same bytes, now Zeroize-on-Drop under
    // SecretString).
    drop(pre_logger_secrets);

    // Remove FORGEJO_TOKEN from the process environment after we've read
    // it. A same-UID process can otherwise see it via /proc/<pid>/environ
    // until the process exits.
    //
    // SAFETY: single-threaded startup; nothing else has read or observed
    // the environment yet. `set_var` / `remove_var` are `unsafe` in edition
    // 2024 because concurrent environment mutation is UB.
    unsafe {
        std::env::remove_var("FORGEJO_TOKEN");
    }

    // Build the shared secret set from the resolved PAT. `Builder::push_
    // secret_string` confines the `expose_secret` call to `redact.rs` —
    // `main.rs` does not need to unwrap the secret itself. Used by:
    //  - the stderr logger (env_logger Target::Pipe)
    //  - the panic hook (replacement for the default hook)
    //  - the HTTP client's error-string scrubber (stdout / MCP isError)
    let secrets = SecretSet::builder().push_secret_string(&cfg.token).build();

    // Promote TOML log_level into RUST_LOG if the operator didn't set it
    // themselves. env_logger picks up RUST_LOG on init.
    //
    // SAFETY: single-threaded startup; no threads have been spawned.
    unsafe {
        promote_log_level(cfg.log_level.as_deref());
    }

    init_logger_with_redaction(secrets.clone());
    install_panic_hook(secrets.clone());

    log_startup_banner(&cfg);

    let client = match ForgejoClient::new(
        cfg.base_url.clone(),
        cfg.token.clone(),
        cfg.user_agent.clone(),
        cfg.timeout_secs,
        secrets,
    ) {
        Ok(c) => c,
        Err(e) => {
            log::error!("failed to build HTTP client: {e:#}");
            return ExitCode::from(2);
        }
    };

    let server = Server::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
        .with_instructions(INSTRUCTIONS)
        .with_tool(ForgejoOpenPrTool {
            client: client.clone(),
        })
        .with_tool(ForgejoGetReviewCommentsTool {
            client: client.clone(),
        })
        .with_tool(ForgejoPostReviewTool {
            client: client.clone(),
        })
        .with_tool(ForgejoAddReviewCommentTool {
            client: client.clone(),
        })
        .with_tool(ForgejoClosePrTool { client });

    match server.run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log::error!("MCP server exited with error: {e:#}");
            ExitCode::from(1)
        }
    }
}

/// Build `env_logger` routed through a [`RedactingWriter`] that scrubs the
/// PAT bytes from everything written to stderr.
///
/// NOTE: `reqwest` / `hyper` are capped at `warn` regardless of any
/// operator `RUST_LOG` setting so we don't dump HTTP headers or bodies
/// into the log machinery in the first place (PAT-leak defense). If
/// operators want more detail from those crates, they should unfilter
/// in this file, not via `RUST_LOG`.
fn init_logger_with_redaction(secrets: SecretSet) {
    let writer = RedactingWriter::new(std::io::stderr(), secrets);
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .filter_module("reqwest", LevelFilter::Warn)
        .filter_module("hyper", LevelFilter::Warn)
        .target(env_logger::Target::Pipe(Box::new(writer)))
        .init();
}

/// Install a panic hook that routes the panic message through a
/// [`RedactingWriter`] wrapping stderr before printing. The default hook
/// writes directly to the stderr fd, bypassing `env_logger`'s writer.
fn install_panic_hook(secrets: SecretSet) {
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let tname = thread.name().unwrap_or("<unnamed>").to_string();
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let msg = info
            .payload()
            .downcast_ref::<&'static str>()
            .copied()
            .map(String::from)
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        let mut w = RedactingWriter::new(std::io::stderr(), secrets.clone());
        write_panic_line(&mut w, &tname, &loc, &msg);
    }));
}

/// Shared panic-formatter; factored so tests can drive it against any
/// `Write` sink without installing a global hook.
fn write_panic_line<W: Write>(w: &mut W, thread_name: &str, loc: &str, msg: &str) {
    let _ = writeln!(w, "thread '{thread_name}' panicked at {loc}:\n{msg}");
}

fn log_startup_banner(cfg: &ResolvedConfig) {
    info!(
        "{} v{} starting (base_url={}, timeout_secs={}, user_agent={})",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        cfg.base_url,
        cfg.timeout_secs,
        cfg.user_agent,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `write_panic_line` feeds a `RedactingWriter`; a panic payload
    /// containing the PAT must come out scrubbed.
    #[test]
    fn write_panic_line_scrubs_pat_through_redacting_writer() {
        let pat = "super-secret-pat-xyz";
        let secrets = SecretSet::new([pat.as_bytes().to_vec()]);
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut w = RedactingWriter::new(&mut sink, secrets);
            write_panic_line(
                &mut w,
                "main",
                "src/client.rs:42:5",
                &format!("state: token={pat} done"),
            );
        }
        let s = String::from_utf8_lossy(&sink);
        assert!(!s.contains(pat), "PAT leaked through hook formatter: {s}");
        assert!(s.contains("<REDACTED>"), "got: {s}");
        assert!(s.contains("thread 'main' panicked"), "got: {s}");
    }
}
