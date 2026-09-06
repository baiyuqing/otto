# Otto development and macOS acceptance checks. See docs/development.md.
# Tests use no live provider credentials; tool/dependency setup may use network.

BINARY := otto
PKG    := ./cmd/otto

STATICCHECK_VERSION := v0.8.1
STATICCHECK := go run honnef.co/go/tools/cmd/staticcheck@$(STATICCHECK_VERSION)
CORE_PACKAGES := ./internal/model ./internal/agent ./internal/app ./internal/provider/... ./internal/config ./internal/skill ./internal/subagent

.PHONY: all build fmt fmt-fix vet lint test test-core test-architecture test-race test-tui check-fast check ui ui-test clean help

all: build

build: ## compile the otto binary (trimmed) to ./$(BINARY)
	go build -trimpath -o ./$(BINARY) $(PKG)

fmt: ## fail if any Go file is not gofmt-formatted (CI-safe)
	@test -z "$$(gofmt -l .)" || { echo "gofmt: unformatted files:"; gofmt -l .; exit 1; }

fmt-fix: ## rewrite Go files with gofmt -w (local only)
	gofmt -w .

vet: ## run go vet
	go vet ./...

lint: ## run the pinned staticcheck version
	$(STATICCHECK) ./...

test: ## run the offline unit test suite
	go test ./...

test-core: ## run focused core tests without host process/Seatbelt suites
	go test $(CORE_PACKAGES)

test-architecture: ## verify production package import boundaries
	go test ./internal/architecture

test-race: ## run the test suite with the race detector
	go test -race -timeout=20m ./...

test-tui: ## run the TUI PTY lifecycle smoke test
	go test ./cmd/otto -run TestTUIPseudoTerminalLifecycle -count=1

# The web UI needs Node and is not part of check/check-fast; `go build`
# embeds whatever internal/server/ui/dist holds (a placeholder when unbuilt).
ui: ## build the web UI into internal/server/ui/dist (needs Node 24+)
	rm -rf internal/server/ui/dist/assets internal/server/ui/dist/index.html
	cd ui && npm ci && npm run build

ui-test: ## run the web UI unit tests (needs Node 24+)
	cd ui && npm ci && npm test

check-fast: fmt vet test-architecture test-core ## quick feedback; run targeted package tests too
	@git diff --check

check: check-fast build lint test test-race test-tui ## full macOS acceptance, including host integration and PTY
	@git diff --check || { echo "git diff --check failed"; exit 1; }

clean: ## remove the built binary
	rm -f ./$(BINARY)

help: ## list targets
	@awk 'BEGIN {FS = ":.*## "; printf "Usage: make [target]\n\n"} /^[a-zA-Z_-]+:.*## / {printf "  %-12s %s\n", $$1, $$2}' $(MAKEFILE_LIST)
