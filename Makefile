# Otto development and macOS acceptance checks. See docs/development.md.
# Tests use no live provider credentials; tool/dependency setup may use network.

BINARY := otto
INSTALL_DIR ?= $(HOME)/.local/bin
SKILLS_INSTALL_DIR ?= $(HOME)/.otto/skills

CONNECT_DIR := connect

DESKTOP_DIR := desktop
DESKTOP_TARGET := aarch64-apple-darwin
DESKTOP_SIDECAR := $(DESKTOP_DIR)/resources/darwin-arm64/bin/otto
DESKTOP_APP := $(DESKTOP_DIR)/build/darwin-arm64/Otto.app

.PHONY: all build install check-fast check check-tests check-linux ui-deps ui ui-test rust-fmt rust-lint rust-test rust-wasm-check rust-wasm-test scripts-test bump release-tag test-tui connect-check connect-build desktop-sidecar desktop-check desktop-app desktop-release clean help

all: build

build: ui ## build the Web UI and compile the Rust binary (release) to ./$(BINARY)
	cargo build --release
	cp target/release/$(BINARY) ./$(BINARY)

install: build connect-build ## install Otto, otto-connect and bundled skills
	install -d "$(INSTALL_DIR)"
	install -m 0755 ./$(BINARY) "$(INSTALL_DIR)/$(BINARY)"
	install -m 0755 target/otto-connect "$(INSTALL_DIR)/otto-connect"
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
	@unformatted="$$(cd $(CONNECT_DIR) && gofmt -l .)"; test -z "$$unformatted" || { echo "gofmt needed: $$unformatted"; exit 1; }
	cd $(CONNECT_DIR) && go vet ./...
	cd $(CONNECT_DIR) && OTTO_BIN="$(CURDIR)/target/debug/otto" go test -race ./...

connect-build: ## build the chat connector to target/otto-connect
	cd $(CONNECT_DIR) && go build -o ../target/otto-connect ./cmd/otto-connect

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

check: rust-fmt rust-lint check-tests ## full macOS acceptance: format, lint, web UI build, all Rust tests (PTY tests included), host integration and web UI tests (no release build; see `build`)

# CI's macOS acceptance job runs this, and a separate job runs rust-fmt and
# rust-lint at the same time.
check-tests: ui rust-test connect-check desktop-check rust-wasm-test scripts-test ui-test ## `check` without rust-fmt and rust-lint
	@git diff --check || { echo "git diff --check failed"; exit 1; }

check-linux: rust-fmt rust-lint rust-test connect-check desktop-check rust-wasm-check scripts-test ## Linux gate: no Seatbelt, so no sandbox conformance; the web UI is platform independent and stays with `check`
	@git diff --check || { echo "git diff --check failed"; exit 1; }

ui-deps: ## build the wasm package and install web UI dependencies (needs Node 24+ and wasm-pack)
	wasm-pack build --target web crates/otto-web
	cd ui && npm ci

ui: ui-deps ## build the web UI into ui/dist (needs Node 24+ and wasm-pack)
	rm -rf ui/dist/assets ui/dist/index.html
	cd ui && npm run build

ui-test: ui-deps ## run the web UI unit tests (needs Node 24+ and wasm-pack)
	cd ui && npm test

desktop-sidecar: ui ## build the web UI and otto for arm64, then copy it to MyGo's platform resources
	cargo build --release --target $(DESKTOP_TARGET) -p otto
	mkdir -p $(dir $(DESKTOP_SIDECAR))
	cp target/$(DESKTOP_TARGET)/release/otto $(DESKTOP_SIDECAR)

desktop-check: ## format, vet, race-test desktop contracts against otto, and cross-compile the macOS arm64 shell
	cargo test --workspace --no-run
	@unformatted="$$(cd $(DESKTOP_DIR) && gofmt -l .)"; test -z "$$unformatted" || { echo "gofmt needed: $$unformatted"; exit 1; }
	cd $(DESKTOP_DIR) && go vet ./...
	cd $(DESKTOP_DIR) && OTTO_BIN="$(CURDIR)/target/debug/otto" go test -race ./...
	cd $(DESKTOP_DIR) && CGO_ENABLED=0 GOOS=darwin GOARCH=arm64 go vet ./...
	cd $(DESKTOP_DIR) && CGO_ENABLED=0 GOOS=darwin GOARCH=arm64 go build -o ../target/otto-desktop .

desktop-release: ## build, sign and notarize the macOS desktop app (needs APPLE_SIGNING_IDENTITY and NOTARY_PROFILE)
	@test -n "$(APPLE_SIGNING_IDENTITY)" || { echo "desktop-release: set APPLE_SIGNING_IDENTITY to a Developer ID Application identity"; exit 1; }
	@test -n "$(NOTARY_PROFILE)" || { echo "desktop-release: set NOTARY_PROFILE to a notarytool keychain profile (see: xcrun notarytool store-credentials)"; exit 1; }
	$(MAKE) desktop-sidecar
	cd $(DESKTOP_DIR) && go run github.com/egoist/mygo/cmd/mygo build -platform darwin/arm64 -skip-dmg -skip-notarize -sign "$(APPLE_SIGNING_IDENTITY)"
	ditto -c -k --keepParent $(DESKTOP_APP) $(DESKTOP_APP).zip
	xcrun notarytool submit $(DESKTOP_APP).zip --keychain-profile "$(NOTARY_PROFILE)" --wait
	xcrun stapler staple $(DESKTOP_APP)

desktop-app: desktop-sidecar ## build Otto.app for this Mac with an ad-hoc signature (no Developer ID or notarization)
	cd $(DESKTOP_DIR) && go run github.com/egoist/mygo/cmd/mygo build -platform darwin/arm64 -skip-dmg -skip-notarize -sign -
	@echo "built $(DESKTOP_APP)"

clean: ## remove the built binary and all Cargo build outputs in target/
	rm -f ./$(BINARY)
	cargo clean

help: ## list targets
	@awk 'BEGIN {FS = ":.*## "; printf "Usage: make [target]\n\n"} /^[a-zA-Z_-]+:.*## / {printf "  %-16s %s\n", $$1, $$2}' $(MAKEFILE_LIST)
