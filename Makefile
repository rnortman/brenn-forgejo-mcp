.PHONY: check rust-check init-check msrv fmt clippy test build release release-musl setup-hooks scrub-tree clean

# Full pre-commit check suite. rust-check is everything except
# init-check, which needs brenn-scrub on PATH.
check: rust-check init-check

rust-check: msrv fmt clippy test

# rust-version claims only the pinned toolchain: it must equal the
# major.minor of rust-toolchain.toml's channel.
msrv:
	@claim=$$(sed -n 's/^rust-version *= *"\(.*\)"/\1/p' Cargo.toml); \
	pin=$$(sed -n 's/^channel *= *"\([0-9]*\.[0-9]*\).*"/\1/p' rust-toolchain.toml); \
	if [ -z "$$claim" ] || [ "$$claim" != "$$pin" ]; then \
		echo "msrv: Cargo.toml rust-version '$$claim' != rust-toolchain.toml channel major.minor '$$pin'" >&2; \
		exit 1; \
	fi

build:
	cargo build

# Production release build.
release:
	cargo build --release

# Static musl release build (for CI/deployment). Needs `musl-gcc` or
# `x86_64-linux-musl-gcc` on PATH: `ring` compiles C and the `cc` crate
# does not fall back to the host compiler for a cross target.
release-musl:
	cargo build --release --target x86_64-unknown-linux-musl

fmt:
	cargo fmt -- --check

clippy:
	cargo clippy --all-targets -- -D warnings

test:
	cargo test

init-check:
	brenn-scrub init --check

setup-hooks:
	git config core.hooksPath .githooks
	@rm -f .git/hooks/pre-commit
	@command -v brenn-scrub >/dev/null 2>&1 || echo "brenn-scrub not on PATH: cargo install --locked brenn-scrub"
	@command -v gitleaks >/dev/null 2>&1 || echo "gitleaks not on PATH: install the release brenn-scrub pins (brenn-scrub reports it)"
	@echo "setup-hooks: done."

scrub-tree:
	brenn-scrub tree

clean:
	cargo clean
