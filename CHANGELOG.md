# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - 2026-10-15

First public release, under the Apache-2.0 license. Development through
0.1.1 was private.

### Changed

- **Renamed from `forgejo-mcp-rs` to `brenn-forgejo-mcp`.** The crate,
  the installed binary, the Rust library path, log/error message prefixes,
  and the default HTTP `User-Agent` (now `brenn-forgejo-mcp/<version>`)
  all use the new name. Update MCP client configs and deploy scripts that
  invoke the old binary name.
- Published on crates.io. The MCP framework dependency is now the
  published `brenn-mcp` crate (same code as before, released name)
  instead of a private git dependency.
- The Rust toolchain is pinned to 1.97.1 (`rust-toolchain.toml`), which
  is also the declared minimum supported Rust version.
- `--help` text, README and project docs no longer describe a specific
  host deployment.

### Added

- Secret-scanning (`brenn-scrub`, `gitleaks`) git hooks, installed with
  `make setup-hooks`, and a GitHub Actions CI workflow.

[0.2.0]: https://github.com/rnortman/brenn-forgejo-mcp/releases/tag/v0.2.0
