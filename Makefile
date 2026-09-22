# Otto development and macOS acceptance checks. See docs/development.md.
# Tests use no live provider credentials; tool/dependency setup may use network.

BINARY := otto
INSTALL_DIR ?= $(HOME)/.local/bin

.PHONY: all build install check-fast check check-linux ui ui-test rust-fmt rust-lint rust-test rust-wasm-check rust-wasm-test test-tui clean help

all: build

build: ui ## build the Web UI and compile the Rust binary (release) to ./$(BINARY)
	cargo build --release
	cp target/release/$(BINARY) ./$(BINARY)

install: build ## install the Otto binary to $(INSTALL_DIR)
	install -d "$(INSTALL_DIR)"
	install -m 0755 ./$(BINARY) "$(INSTALL_DIR)/$(BINARY)"

rust-fmt: ## fail if any Rust file is not rustfmt-formatted
	cargo fmt --all -- --check

rust-lint: ## run clippy across the Rust workspace with warnings as errors
	cargo clippy --workspace --all-targets -- -D warnings

rust-test: ## run the full native Rust test suite
	cargo test --workspace

rust-wasm-check: ## verify otto-core and otto-web still build for wasm32
	cargo check -p otto-core --target wasm32-unknown-unknown
	cargo check -p otto-web --target wasm32-unknown-unknown

rust-wasm-test: ## run the Rust wasm tests under Node (needs wasm-pack)
	wasm-pack test --node crates/otto-core
	wasm-pack test --node crates/otto-web

test-tui: ## run the TUI PTY lifecycle smoke test (needs a real PTY)
	cargo test -p otto --test tui_pty

check-fast: rust-fmt rust-lint ## quick feedback: formatting, lint, focused core tests
	cargo test -p otto-core
	@git diff --check

check: check-fast build rust-test rust-wasm-check rust-wasm-test test-tui ui-test ## full macOS acceptance, including host integration, PTY and web UI tests
	@git diff --check || { echo "git diff --check failed"; exit 1; }

check-linux: rust-fmt rust-lint rust-test rust-wasm-check test-tui ## Linux gate: no Seatbelt, so no sandbox conformance; the web UI is platform independent and stays with `check`
	@git diff --check || { echo "git diff --check failed"; exit 1; }

ui: ## build the web UI into ui/dist (needs Node 24+ and wasm-pack)
	rm -rf ui/dist/assets ui/dist/index.html
	wasm-pack build --target web crates/otto-web
	cd ui && npm ci && npm run build

ui-test: ## run the web UI unit tests (needs Node 24+ and wasm-pack)
	wasm-pack build --target web crates/otto-web
	cd ui && npm ci && npm test

clean: ## remove the built binary
	rm -f ./$(BINARY)

help: ## list targets
	@awk 'BEGIN {FS = ":.*## "; printf "Usage: make [target]\n\n"} /^[a-zA-Z_-]+:.*## / {printf "  %-16s %s\n", $$1, $$2}' $(MAKEFILE_LIST)
