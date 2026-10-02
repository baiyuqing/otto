# Otto development and macOS acceptance checks. See docs/development.md.
# Tests use no live provider credentials; tool/dependency setup may use network.

BINARY := otto
INSTALL_DIR ?= $(HOME)/.local/bin
SKILLS_INSTALL_DIR ?= $(HOME)/.otto/skills

CONNECT_DIR := connect

DESKTOP_DIR := desktop/src-tauri
DESKTOP_TARGET := aarch64-apple-darwin
DESKTOP_SIDECAR := $(DESKTOP_DIR)/binaries/otto-$(DESKTOP_TARGET)
DESKTOP_APP := $(DESKTOP_DIR)/target/$(DESKTOP_TARGET)/release/bundle/macos/Otto.app
# `cargo tauri build` runs `xattr -crs` on the bundle; /usr/bin comes first so
# a pip-installed `xattr` earlier on PATH, which has no -r, is not used.

.PHONY: all build install check-fast check check-linux ui ui-test rust-fmt rust-lint rust-test rust-wasm-check rust-wasm-test scripts-test test-tui connect-check connect-build desktop-sidecar desktop-check desktop-app desktop-release clean help

all: build

build: ui ## build the Web UI and compile the Rust binary (release) to ./$(BINARY)
	cargo build --release
	cp target/release/$(BINARY) ./$(BINARY)

install: build ## install Otto and bundled skills
	install -d "$(INSTALL_DIR)"
	install -m 0755 ./$(BINARY) "$(INSTALL_DIR)/$(BINARY)"
	install -d "$(SKILLS_INSTALL_DIR)/sandbox-setup"
	install -m 0644 .otto/skills/sandbox-setup/SKILL.md "$(SKILLS_INSTALL_DIR)/sandbox-setup/SKILL.md"

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

connect-check: ## Go chat connector: gofmt, vet, race tests; builds target/debug/otto for its end-to-end test
	cargo build -p otto
	@unformatted="$$(cd $(CONNECT_DIR) && gofmt -l .)"; test -z "$$unformatted" || { echo "gofmt needed: $$unformatted"; exit 1; }
	cd $(CONNECT_DIR) && go vet ./...
	cd $(CONNECT_DIR) && OTTO_BIN="$(CURDIR)/target/debug/otto" go test -race ./...

connect-build: ## build the chat connector to target/otto-connect
	cd $(CONNECT_DIR) && go build -o ../target/otto-connect ./cmd/otto-connect

check-fast: rust-fmt rust-lint ## quick feedback: formatting, lint, focused core tests
	cargo test -p otto-core
	@git diff --check

check: check-fast build rust-test connect-check rust-wasm-check rust-wasm-test scripts-test test-tui ui-test ## full macOS acceptance, including host integration, PTY and web UI tests
	@git diff --check || { echo "git diff --check failed"; exit 1; }

check-linux: rust-fmt rust-lint rust-test connect-check rust-wasm-check scripts-test test-tui ## Linux gate: no Seatbelt, so no sandbox conformance; the web UI is platform independent and stays with `check`
	@git diff --check || { echo "git diff --check failed"; exit 1; }

ui: ## build the web UI into ui/dist (needs Node 24+ and wasm-pack)
	rm -rf ui/dist/assets ui/dist/index.html
	wasm-pack build --target web crates/otto-web
	cd ui && npm ci && npm run build

ui-test: ## run the web UI unit tests (needs Node 24+ and wasm-pack)
	wasm-pack build --target web crates/otto-web
	cd ui && npm ci && npm test

desktop-sidecar: ## build otto for arm64 and copy it to the path Tauri bundles it from
	cargo build --release --target $(DESKTOP_TARGET) -p otto
	mkdir -p $(DESKTOP_DIR)/binaries
	cp target/$(DESKTOP_TARGET)/release/otto $(DESKTOP_SIDECAR)

desktop-check: desktop-sidecar ## build, lint and test the macOS desktop shell (arm64; not part of `make check`)
	cd $(DESKTOP_DIR) && cargo fmt --all -- --check
	cd $(DESKTOP_DIR) && cargo clippy --all-targets -- -D warnings
	cd $(DESKTOP_DIR) && cargo test

desktop-release: ## build, sign and notarize the macOS desktop app (needs APPLE_SIGNING_IDENTITY, NOTARY_PROFILE, and cargo-tauri)
	@test -n "$(APPLE_SIGNING_IDENTITY)" || { echo "desktop-release: set APPLE_SIGNING_IDENTITY to a Developer ID Application identity"; exit 1; }
	@test -n "$(NOTARY_PROFILE)" || { echo "desktop-release: set NOTARY_PROFILE to a notarytool keychain profile (see: xcrun notarytool store-credentials)"; exit 1; }
	$(MAKE) desktop-sidecar
	cd $(DESKTOP_DIR) && PATH=/usr/bin:$$PATH APPLE_SIGNING_IDENTITY="$(APPLE_SIGNING_IDENTITY)" cargo tauri build --target $(DESKTOP_TARGET)
	ditto -c -k --keepParent $(DESKTOP_APP) $(DESKTOP_APP).zip
	xcrun notarytool submit $(DESKTOP_APP).zip --keychain-profile "$(NOTARY_PROFILE)" --wait
	xcrun stapler staple $(DESKTOP_APP)

desktop-app: desktop-sidecar ## build Otto.app for this Mac with an ad-hoc signature (needs cargo-tauri; no Developer ID, no notarization)
	cd $(DESKTOP_DIR) && PATH=/usr/bin:$$PATH APPLE_SIGNING_IDENTITY=- cargo tauri build --target $(DESKTOP_TARGET)
	@echo "built $(DESKTOP_APP)"

clean: ## remove the built binary
	rm -f ./$(BINARY)

help: ## list targets
	@awk 'BEGIN {FS = ":.*## "; printf "Usage: make [target]\n\n"} /^[a-zA-Z_-]+:.*## / {printf "  %-16s %s\n", $$1, $$2}' $(MAKEFILE_LIST)
