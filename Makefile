# Otto build and CI targets.
#
# These mirror the gates documented in AGENTS.md ("Go workflow") and
# README.md ("Build and test commands"). The default suite must stay
# offline: it needs no provider credentials, network access, or an
# interactive terminal.

BINARY := otto
PKG    := ./cmd/otto

STATICCHECK := go run honnef.co/go/tools/cmd/staticcheck@latest

.PHONY: all build fmt fmt-fix vet lint test test-race test-tui check clean help

all: build

build: ## compile the otto binary (trimmed) to ./$(BINARY)
	go build -trimpath -o ./$(BINARY) $(PKG)

fmt: ## fail if any Go file is not gofmt-formatted (CI-safe)
	@test -z "$$(gofmt -l .)" || { echo "gofmt: unformatted files:"; gofmt -l .; exit 1; }

fmt-fix: ## rewrite Go files with gofmt -w (local only)
	gofmt -w .

vet: ## run go vet
	go vet ./...

lint: ## run staticcheck (pinned via @latest in go run form)
	$(STATICCHECK) ./...

test: ## run the offline unit test suite
	go test ./...

test-race: ## run the test suite with the race detector
	go test -race ./...

test-tui: ## run the TUI PTY lifecycle smoke test
	go test ./cmd/otto -run TestTUIPseudoTerminalLifecycle -count=1

check: fmt vet lint test test-race ## run every CI gate (fmt, vet, lint, test, test-race, diff)
	@git diff --check || { echo "git diff --check failed"; exit 1; }

clean: ## remove the built binary
	rm -f ./$(BINARY)

help: ## list targets
	@awk 'BEGIN {FS = ":.*## "; printf "Usage: make [target]\n\n"} /^[a-zA-Z_-]+:.*## / {printf "  %-12s %s\n", $$1, $$2}' $(MAKEFILE_LIST)
