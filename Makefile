# Otto development and macOS acceptance checks. See docs/development.md.
# Tests use no live provider credentials; tool/dependency setup may use network.

BINARY := otto
INSTALL_DIR ?= $(HOME)/.local/bin
SKILLS_INSTALL_DIR ?= $(HOME)/.otto/skills

CHAT_CONNECTOR_DIR := chat-connector

.PHONY: setup-git-hooks all build run install check-fast check check-tests check-linux rust-fmt rust-lint rust-test rust-wasm-check rust-wasm-test scripts-test license-check bump release-tag test-tui connect-check connect-build clean help

all: build

build: ## compile the Rust binary (release) to ./$(BINARY)
	cargo build --release
	cp target/release/$(BINARY) ./$(BINARY)

run: ## start Otto in the foreground with the settings from otto.env (run `build` first)
	./scripts/otto.sh

install: build connect-build ## install Otto, otto-connect and bundled skills
	install -d "$(INSTALL_DIR)"
	install -m 0755 ./$(BINARY) "$(INSTALL_DIR)/$(BINARY)"
	install -m 0755 target/otto-connect "$(INSTALL_DIR)/otto-connect"
	install -d "$(SKILLS_INSTALL_DIR)/sandbox-setup"
	install -m 0644 .otto/skills/sandbox-setup/SKILL.md "$(SKILLS_INSTALL_DIR)/sandbox-setup/SKILL.md"

setup-git-hooks: ## install the Gitleaks pre-commit hook for this clone and its worktrees
	@command -v gitleaks >/dev/null || { echo "Install gitleaks first (macOS: brew install gitleaks)"; exit 1; }
	@hooks="$$(git rev-parse --git-path hooks)"; \
	if [ -L "$$hooks/pre-commit" ] || { [ -e "$$hooks/pre-commit" ] && ! cmp -s scripts/pre-commit "$$hooks/pre-commit"; }; then \
	  echo "Existing pre-commit hook differs; integrate scripts/pre-commit without overwriting it"; exit 1; \
	fi; \
	mkdir -p "$$hooks" && install -m 0755 scripts/pre-commit "$$hooks/pre-commit"

rust-fmt: ## fail if any Rust file is not rustfmt-formatted
	cargo fmt --all -- --check

rust-lint: ## run clippy across the Rust workspace with warnings as errors
	cargo clippy --workspace --all-targets -- -D warnings

rust-test: ## run the full native Rust test suite
	cargo test --workspace

rust-wasm-check: ## verify otto-core still builds for wasm32
	cargo check -p otto-core --target wasm32-unknown-unknown

rust-wasm-test: ## run the Rust wasm tests under Node (needs wasm-pack)
	wasm-pack test --node crates/otto-core

scripts-test: ## run the Node tests beside scripts/ (offline, no build needed)
	node --test scripts/*.test.mjs

license-check: ## fail if a shipped Rust crate uses a license outside the allowlist in scripts/license-check.mjs
	node scripts/license-check.mjs

bump: ## set a new release version in every manifest and lockfile (VERSION=X.Y.Z)
	@test -n "$(VERSION)" || { echo "usage: make bump VERSION=X.Y.Z"; exit 1; }
	node scripts/release.mjs bump $(VERSION)

release-tag: ## tag origin/main with the Cargo.toml version, push the tag and create the GitHub release (needs gh)
	node scripts/release.mjs tag

test-tui: ## run only the TUI PTY tests, local and --attach (rust-test runs them too)
	cargo test -p otto --test tui_pty --test tui_attach_pty

# `cargo test --workspace --no-run` rather than `cargo build -p otto`: both
# leave target/debug/otto, but only this one resolves the same features as
# rust-test, so in a gate it reuses that build instead of compiling the whole
# dependency tree and otto again (a `cargo build` leaves dev-dependency
# features off, which changes even proc-macro crates).
connect-check: ## Go chat connector: gofmt, vet, race tests; builds target/debug/otto for its end-to-end test
	cargo test --workspace --no-run
	@unformatted="$$(cd $(CHAT_CONNECTOR_DIR) && gofmt -l .)"; test -z "$$unformatted" || { echo "gofmt needed: $$unformatted"; exit 1; }
	cd $(CHAT_CONNECTOR_DIR) && go vet ./...
	cd $(CHAT_CONNECTOR_DIR) && OTTO_BIN="$(CURDIR)/target/debug/otto" go test -race ./...

connect-build: ## build the chat connector to target/otto-connect
	cd $(CHAT_CONNECTOR_DIR) && go build -o ../target/otto-connect ./cmd/otto-connect

# check-fast is the local gate. All worktrees of this repository share one lock
# file in the common git directory, so check-fast in several worktrees runs one
# at a time instead of each compiling on every core. lockf is the macOS tool,
# flock the Linux one; both release the lock when the process exits.
CHECK_LOCK = $(shell git rev-parse --path-format=absolute --git-common-dir)/otto-check-fast.lock
ifeq ($(shell uname -s),Darwin)
CHECK_LOCKED = lockf -k "$(CHECK_LOCK)"
else
CHECK_LOCKED = flock "$(CHECK_LOCK)"
endif

check-fast: ## local gate: only the checks for paths changed since origin/main, one worktree at a time
	@echo "check-fast: taking $(CHECK_LOCK); waits while another worktree's check-fast holds it"
	@$(CHECK_LOCKED) node scripts/check-changed.mjs

check: rust-fmt rust-lint check-tests ## full macOS acceptance: format, lint, all Rust tests (PTY tests included), host integration (no release build; see `build`)

# CI's macOS acceptance job runs this, and a separate job runs rust-fmt and
# rust-lint at the same time.
check-tests: rust-test connect-check rust-wasm-test scripts-test ## `check` without rust-fmt and rust-lint
	@git diff --check || { echo "git diff --check failed"; exit 1; }

check-linux: rust-fmt rust-lint rust-test connect-check rust-wasm-check scripts-test license-check ## Linux gate: no Seatbelt, so no sandbox conformance
	@git diff --check || { echo "git diff --check failed"; exit 1; }

clean: ## remove the built binary and all Cargo build outputs in target/
	rm -f ./$(BINARY)
	cargo clean

help: ## list targets
	@awk 'BEGIN {FS = ":.*## "; printf "Usage: make [target]\n\n"} /^[a-zA-Z_-]+:.*## / {printf "  %-16s %s\n", $$1, $$2}' $(MAKEFILE_LIST)
