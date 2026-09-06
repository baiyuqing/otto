# Web UI as a client of otto serve

Status: approved 2026-09-06. Records the decisions behind the loopback TCP
listener, the bearer token, `POST .../compact`, and the embedded browser UI.
Current behavior is documented in the [user manual](../user-manual.md#agent-server).

## Goal

Otto's end state is an agent that runs as a server, on this machine or
elsewhere, and any interactive frontend is a client of that server. The
inline Bubble Tea TUI depends on real-terminal cursor placement (required for
macOS IME candidate windows, see PR #9) plus manual row arithmetic in
`internal/tui/model.go`, and the ultraviolet inline renderer clamps the
cursor row in ways that PRs #56, #64, and #77 worked around one at a time. A
browser UI has none of those constraints and needs nothing from the process
except HTTP. `internal/server` already exposes sessions, turns, SSE events,
and tasks over a Unix socket; this design adds what a browser needs.

## Usage model

One `otto serve` process per workspace. Each process serves its own UI and
listens on its own loopback port; one browser tab per process. There is no
multi-process hub, discovery, or CORS. The TUI and REPL are left in place.

## Decisions

1. **Listener.** `--listen HOST:PORT` and `[server].listen` select TCP
   instead of the socket; exactly one listener is opened. Only loopback hosts
   are accepted (`127.0.0.1`, `::1`, literal `localhost` mapped to
   `127.0.0.1` with no DNS lookup). The loopback check is one function,
   `loopbackHost` in `internal/server/listen_tcp.go`, so a later non-loopback
   mode behind TLS changes that function and nothing else. Port `0` is
   allowed; the resolved URL is printed to stdout.
2. **Token.** TCP mode generates one random token per process and requires
   `Authorization: Bearer <token>` on every `/v1/` route. Any page a browser
   has open can send requests to a loopback port, but the browser never
   attaches this header for another origin, so only the page that received
   the startup URL can call the API. That covers CSRF and DNS rebinding
   without Origin checks. The token is header-only: a query parameter would
   reach proxy access logs and Referer headers once the server is deployed
   behind anything. The token is compared in constant time, never logged, and
   not persisted. The socket listener has no token; its file modes are the
   access control. `/`, `/assets/`, `/healthz`, and `/metrics` are open.
   The check is applied per route in `buildMux`, not in `instrument`, so a
   401 is logged and measured under its real route pattern.
3. **Compact endpoint.** `POST /v1/sessions/{id}/compact` calls
   `Controller.Compact` and returns the same payload as the
   `compaction_completed` event. `startTurn` checked only `os.turn`, and
   `Controller.beginOperation` refuses the second operation only after the
   first has been admitted, so `openSession` gained `compactCancel`; both
   `startTurn` and the compact handler test it under `os.mu`. `DELETE` and
   `Close` cancel it so `ctrl.Close` does not wait on a provider call. The
   client disconnecting cancels the compaction via `context.AfterFunc`.
4. **Static UI.** `internal/server/ui/dist` is embedded with
   `//go:embed all:ui/dist`; only `.gitkeep` is tracked, `make ui` writes the
   rest. A binary built from a clean checkout answers `/` with a one-line
   placeholder. Routes are `GET /{$}` (exact, so unknown paths still 404) and
   `GET /assets/`; neither is in `routeTable` or `openapi.yaml` because they
   are not API. `make check` and CI stay pure Go.
5. **UI stack.** `ui/` is Vite + React + TypeScript with `react`,
   `react-dom`, `react-markdown`, and `remark-gfm`. The client reads SSE with
   `fetch` and a hand-written frame parser rather than `EventSource`, which
   cannot send the Authorization header and reconnects forever after the
   server closes a finished turn's stream. Unit tests cover only the
   SSE-frame-to-transcript reducer.

## Out of scope

Non-loopback binds and TLS, token persistence or rotation, a Linux sandbox
driver (cloud hosts are Linux; the sandbox has only a Seatbelt driver),
autonomous triggers beyond the sub-agent wake loop, profile switching,
archive and memory endpoints, streaming compaction progress, SSE heartbeats,
and `/metrics` authentication. Each needs its own design.
