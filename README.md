# brenn-forgejo-mcp

A small stdio MCP server that exposes Forgejo pull-request operations to MCP
clients such as Claude Code. Synchronous, no async runtime, built on
`brenn-mcp`.

## Tools

- `forgejo_open_pr` — open a pull request
- `forgejo_get_review_comments` — list reviews on a PR together with their
  inline comments
- `forgejo_post_review` — post a review (approve / request changes / comment),
  optionally with inline comments
- `forgejo_add_review_comment` — append an inline comment to an existing PR
  review (Forgejo's REST API has no threaded-reply primitive; the UI groups
  comments on the same file+line, so posting at the same position reads as a
  reply)
- `forgejo_get_pr_comments` — read a PR's conversation thread (comments typed
  into the PR's comment box, not review bodies or inline comments)
- `forgejo_list_prs` — list a repo's PRs by state (`open` / `closed` / `all`),
  optionally filtered by head branch; each entry is a subset of the PR object
  including `number`, `head.sha` and `base.ref`
- `forgejo_get_pr` — fetch one PR's full object
- `forgejo_update_pr` — edit a PR's title, body or base branch (`base` must be
  changed in its own call, without `title` or `body`)
- `forgejo_close_pr` — close a pull request (not merge)

## Install

```
cargo install brenn-forgejo-mcp
```

From a source checkout:

```
cargo install --path .
make release-musl   # static binary at target/x86_64-unknown-linux-musl/release/brenn-forgejo-mcp
```

The static musl build needs a musl-targeting C compiler on `PATH` —
`musl-gcc` from the distribution's musl package (Fedora: `musl-gcc`;
Debian/Ubuntu: `musl-tools`) — because `ring` compiles C; without it the
build stops in `ring`'s build script.

## Usage

```
brenn-forgejo-mcp --config /etc/brenn-forgejo-mcp/config.toml
brenn-forgejo-mcp --base-url https://forgejo.example.com   # reads FORGEJO_TOKEN from env
```

The server speaks newline-delimited JSON-RPC (MCP) on stdin/stdout and logs
to stderr.

## Config file

```toml
base_url = "https://forgejo.example.com"   # required (here or via --base-url)
token    = "abc123..."                     # required (here or via FORGEJO_TOKEN env var)
# Optional:
timeout_secs = 30
log_level    = "info"
# user_agent = "..."   # default: brenn-forgejo-mcp/<installed version>
```

`base_url` and `token` may each come from exactly one source. Setting both
TOML `token` and `FORGEJO_TOKEN` is a configuration bug and fails startup.
`--base-url` overrides TOML `base_url`.

On Unix the config file must be mode 0600 or stricter: if any group or other
permission bit is set, startup fails with a "permissions too open" error.

## Claude Code wiring

In `.mcp.json`:

```json
{
  "mcpServers": {
    "forgejo": {
      "command": "brenn-forgejo-mcp",
      "args": ["--config", "/etc/brenn-forgejo-mcp/config.toml"]
    }
  }
}
```

`brenn-forgejo-mcp` is on `PATH` after `cargo install`; if your client does
not inherit `PATH`, give the absolute path instead.

To pass the token through the environment instead of the config file:

```json
{
  "mcpServers": {
    "forgejo": {
      "command": "brenn-forgejo-mcp",
      "args": ["--base-url", "https://forgejo.example.com"],
      "env": { "FORGEJO_TOKEN": "..." }
    }
  }
}
```

Or from the command line:

```
claude mcp add forgejo -- brenn-forgejo-mcp --config /etc/brenn-forgejo-mcp/config.toml
```

Any other MCP client wires the same three fields: command, arguments and
environment.

## Error semantics

- Forgejo 4xx responses come back as an `isError: true` tool result
  prefixed `Forgejo API error (XXX):` — an input or auth problem; retrying
  without changing something will not help.
- Forgejo 5xx responses and network errors come back prefixed
  `Forgejo transient error:` — safe to retry. 5xx bodies are dropped; only
  the status code is reported.
- Malformed tool input comes back as `isError: true`.
- A 2xx response whose body is not JSON at all means Forgejo changed its API
  or the server hit the wrong endpoint. The process panics and exits rather
  than guess at a response it cannot parse; whether your MCP client
  relaunches it is the client's policy.
- An invalid configuration fails startup.

No HTTP request or response bodies are logged at any level, and the access
token is scrubbed from everything written to stderr and from error text
returned to the client.

## Building

```
make check        # msrv + fmt + clippy + test + brenn-scrub init --check
make release-musl # static musl release build (needs `musl-gcc` on `PATH`, see Install)
make setup-hooks  # enable the brenn-scrub pre-commit/pre-push hooks
```

`make check` and the hooks need
[`brenn-scrub`](https://crates.io/crates/brenn-scrub) and the `gitleaks`
release it pins on `PATH` (`cargo install --locked brenn-scrub`).
`make rust-check` runs the Rust checks alone.

## Inline-comment field naming

Forgejo's read and write schemas for review comments are not symmetric:

| Concept                    | Read (`forgejo_get_review_comments`) | Write (`forgejo_post_review`, `forgejo_add_review_comment`) |
|----------------------------|--------------------------------------|-------------------------------------------------------------|
| Line on the post-change side | `position`                         | `new_position`                                              |
| Line on the old side        | `original_position`                 | `old_position`                                              |

We pass the fields through verbatim in both directions. The write-side names
(`new_position` / `old_position`) are shared by `forgejo_post_review`'s inline
`comments[]` entries and by `forgejo_add_review_comment`'s top-level payload,
so the two tools have identical position-validation semantics (exactly one of
the two must be set).

## License

Licensed under the Apache License, Version 2.0. See [`LICENSE`](LICENSE) and
[`NOTICE`](NOTICE).
