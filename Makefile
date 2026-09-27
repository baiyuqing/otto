# Otto development and macOS acceptance checks. See docs/development.md.
# Tests use no live provider credentials; tool/dependency setup may use network.

BINARY := otto
INSTALL_DIR ?= $(HOME)/.local/bin

DESKTOP_DIR := desktop/src-tauri
DESKTOP_TARGET := aarch64-apple-darwin
DESKTOP_SIDECAR := $(DESKTOP_DIR)/binaries/otto-$(DESKTOP_TARGET)
DESKTOP_APP := $(DESKTOP_DIR)/target/$(DESKTOP_TARGET)/release/bundle/macos/Otto.app

.PHONY: all build install check-fast check check-linux ui ui-test rust-fmt rust-lint rust-test rust-wasm-check rust-wasm-test scripts-test test-tui desktop-check desktop-release clean help

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

scripts-test: ## run the Node tests beside scripts/ (offline, no build needed)
	node --test scripts/*.test.mjs

test-tui: ## run the TUI PTY lifecycle smoke test (needs a real PTY)
	cargo test -p otto --test tui_pty

check-fast: rust-fmt rust-lint ## quick feedback: formatting, lint, focused core tests
	cargo test -p otto-core
	@git diff --check

check: check-fast build rust-test rust-wasm-check rust-wasm-test scripts-test test-tui ui-test ## full macOS acceptance, including host integration, PTY and web UI tests
	@git diff --check || { echo "git diff --check failed"; exit 1; }

check-linux: rust-fmt rust-lint rust-test rust-wasm-check scripts-test test-tui ## Linux gate: no Seatbelt, so no sandbox conformance; the web UI is platform independent and stays with `check`
	@git diff --check || { echo "git diff --check failed"; exit 1; }

ui: ## build the web UI into ui/dist (needs Node 24+ and wasm-pack)
	rm -rf ui/dist/assets ui/dist/index.html
	wasm-pack build --target web crates/otto-web
	cd ui && npm ci && npm run build

ui-test: ## run the web UI unit tests (needs Node 24+ and wasm-pack)
	wasm-pack build --target web crates/otto-web
	cd ui && npm ci && npm test

desktop-check: ## build, lint and test the macOS desktop shell (arm64; not part of `make check`)
	cargo build --release --target $(DESKTOP_TARGET) -p otto
	mkdir -p $(DESKTOP_DIR)/binaries
	cp target/$(DESKTOP_TARGET)/release/otto $(DESKTOP_SIDECAR)
	cd $(DESKTOP_DIR) && cargo fmt --all -- --check
	cd $(DESKTOP_DIR) && cargo clippy --all-targets -- -D warnings
	cd $(DESKTOP_DIR) && cargo test

desktop-release: ## build, sign and notarize the macOS desktop app (needs APPLE_SIGNING_IDENTITY, NOTARY_PROFILE, and cargo-tauri)
	@test -n "$(APPLE_SIGNING_IDENTITY)" || { echo "desktop-release: set APPLE_SIGNING_IDENTITY to a Developer ID Application identity"; exit 1; }
	@test -n "$(NOTARY_PROFILE)" || { echo "desktop-release: set NOTARY_PROFILE to a notarytool keychain profile (see: xcrun notarytool store-credentials)"; exit 1; }
	cargo build --release --target $(DESKTOP_TARGET) -p otto
	mkdir -p $(DESKTOP_DIR)/binaries
	cp target/$(DESKTOP_TARGET)/release/otto $(DESKTOP_SIDECAR)
	cd $(DESKTOP_DIR) && APPLE_SIGNING_IDENTITY="$(APPLE_SIGNING_IDENTITY)" cargo tauri build --target $(DESKTOP_TARGET)
	ditto -c -k --keepParent $(DESKTOP_APP) $(DESKTOP_APP).zip
	xcrun notarytool submit $(DESKTOP_APP).zip --keychain-profile "$(NOTARY_PROFILE)" --wait
	xcrun stapler staple $(DESKTOP_APP)

clean: ## remove the built binary
	rm -f ./$(BINARY)

help: ## list targets
	@awk 'BEGIN {FS = ":.*## "; printf "Usage: make [target]\n\n"} /^[a-zA-Z_-]+:.*## / {printf "  %-16s %s\n", $$1, $$2}' $(MAKEFILE_LIST)
