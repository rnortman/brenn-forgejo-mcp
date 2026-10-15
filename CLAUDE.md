# brenn-forgejo-mcp

A standalone stdio MCP server that exposes nine Forgejo pull-request tools:
`forgejo_open_pr`, `forgejo_list_prs`, `forgejo_get_pr`,
`forgejo_get_review_comments`, `forgejo_get_pr_comments`,
`forgejo_post_review`, `forgejo_add_review_comment`, `forgejo_update_pr`,
`forgejo_close_pr`. Built on the `brenn-mcp`
crate (its docs cover the `Tool` trait and the stdio JSON-RPC loop). We are
the MCP subprocess — we do not spawn one.

## Robustness posture: Better dead than wrong

Robust does not mean "takes a licking and keeps on ticking." Robust means
does not do the wrong thing, ever; shoots itself in the head before it
allows the wrong thing to happen.

- **Fail fast on errors.** Panic rather than tolerate a bug.
- **No fallbacks. No swallowing errors. No "keep calm and carry on."**
- Every `unwrap()` is a conscious decision. Every `?` is intentional.

Applied here:

- **Protocol-break** — a 2xx response body that isn't valid JSON at all —
  **panics**. Forgejo changed the API or we hit the wrong endpoint; the
  process exits. Whether the MCP client relaunches a dead server is the
  client's policy, not ours.
- **Forgejo 4xx / 5xx / network errors** surface through MCP as
  `isError: true` text with a prefix-classified message
  (`Forgejo API error (XXX): ...` vs `Forgejo transient error: ...`).
  Claude uses the prefix to decide whether to retry.
- **Malformed tool input** surfaces through MCP as `isError: true`.
- **Config load failure** exits with status 2 at startup, printing a
  scrubbed error to stderr. It does not panic: the redacting logger and
  panic hook are not installed yet, so a panic would print the
  unscrubbed error (a TOML parse error can quote the `token` line).

## Build / check / deploy

- `make check` — full pre-commit suite (msrv + fmt + clippy + test +
  `brenn-scrub init --check`). `make rust-check` is the same minus the
  scrub-kit check.
- `make setup-hooks` — point `core.hooksPath` at `.githooks` (brenn-scrub
  pre-commit and pre-push hooks) and report missing `brenn-scrub`/`gitleaks`.
- `make scrub-tree` — `brenn-scrub tree` over every tracked file.
- `make release-musl` — static musl release build (needs a musl C compiler,
  `musl-gcc`, on `PATH`).
- `deploy/deploy.sh` — per-host deploy script; see `.forgejo/workflows/ci.yml`.

## PAT handling invariant — two in-process layers

The PAT must never reach stderr or stdout in plaintext. Two layers defend
this, both in code:

1. **`secrecy::SecretString`** wraps the token on `ResolvedConfig` and
   `ForgejoClient`. Its `Debug` and `Display` impls print `[REDACTED]`, so
   a casual `debug!("{:?}", client)` or `error!("{}", token)` emits the
   placeholder, not the secret. Plaintext is reachable only through
   `ExposeSecret::expose_secret()`.

   The `expose_secret()` call sites are confined to two code regions:

   - **`src/client.rs`** — `ForgejoClient::send_with_total` builds the
     `Authorization` header. This is the *only* production site that
     materializes the PAT as HTTP-bound bytes.
   - **`src/redact.rs`** — `SecretSet::Builder::push_secret_string` and
     the private `SecretSet::slices` / `SecretSet::max_len` / `SecretSet::new`
     helpers read the bytes for scrubbing and length comparisons. These
     are the scrubber's own internals; the `Builder::push_secret_string`
     entry point is the documented boundary where `&SecretString` becomes
     scrub-set bytes.

   Any `expose_secret()` call site outside those two files is a red flag — it
   means a new caller is materializing the PAT somewhere new, which needs
   justification.

2. **`RedactingWriter`** (`src/redact.rs`) wraps `io::stderr()` and is
   installed as `env_logger`'s `Target::Pipe` at startup in `main.rs`
   immediately after config resolution. Every byte that flows through the
   log sink — log lines, panic messages routed through the installed
   panic hook, etc. — is scanned for the PAT byte sequence and replaced
   with `<REDACTED>` before reaching stderr. Chunk-boundary-straddling
   occurrences are handled via a rolling carry buffer; trailing bytes
   flush on drop.

   The same `SecretSet` also scrubs MCP `isError` content on stdout: when
   `classify_response` formats a Forgejo 4xx error string that might echo
   a proxy's dump of our request headers, the error string is scrubbed
   before being embedded in `anyhow!`. 5xx bodies are dropped entirely
   (status code only) — upstream proxy/WAF error pages are the realistic
   echo vehicle, and status alone is enough for Claude's retry decision.

These are the actual defense. An earlier plan used a CI `grep` guard;
that was theater (trivially defeated by indirection) and is gone.

No HTTP request or response bodies are logged at any level. If a future
feature wants richer error diagnostics, it must be reviewed against the
no-body-logging invariant.

Any change to `ResolvedConfig::token`'s type, to `ForgejoClient::token`'s
type, to the `expose_secret()` call sites listed above, or to the logger
initialization in `main.rs` must re-validate the invariant.
