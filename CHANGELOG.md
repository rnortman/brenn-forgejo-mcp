# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `forgejo_get_pr_comments`: read a PR's conversation thread — the
  comments typed into the main comment box, as opposed to inline review
  comments — optionally only those updated since a given time. Previously
  this feedback was invisible to the agent.
- `forgejo_list_prs` and `forgejo_get_pr`: find PRs again in a later
  session. List by state, optionally filtered by head branch, with each
  PR's number, head commit SHA and base branch; or fetch one PR in full.
  Very long listings (over 10 pages) are reported as truncated.
- `forgejo_update_pr`: edit a PR's title, description and base branch.
  Changing the base must be done in a call of its own; if Forgejo ignores
  a base change (as it does for merged PRs), the tool reports an error.

### Fixed

- `forgejo_get_review_comments` could silently return an incomplete list
  on PRs with many reviews. Paged listings now use Forgejo's reported
  total count to decide when they are done; a Forgejo response missing
  that count stops the server rather than returning a partial answer.
- `forgejo_open_pr` and `forgejo_post_review` now reject a `body` that is
  not a string, and `forgejo_get_review_comments` a non-string `state`,
  instead of quietly ignoring the field.

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
