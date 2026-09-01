# ChatGPT Subscription Authorization Design

**Status:** Implemented (2026-09-01) on branch `chatgpt-subscription-auth`
**Date:** 2026-09-01
**Target:** Otto Stage 2 (this document does not change Stage 1 scope)

## 1. Summary

Let Otto authorize requests with a user's ChatGPT Plus/Pro/Team/Enterprise
subscription instead of a pay-per-token API key. The mechanism is OpenAI's
"Sign in with ChatGPT" OAuth flow (the same one the Codex CLI uses): a browser
PKCE round-trip that yields access and refresh tokens, stored locally and
refreshed on expiry, then used to authorize requests against the ChatGPT
backend.

This is the "Codex subscription support" item listed under "Planned later" in
`AGENTS.md`. It is Stage 2 work. Nothing here should be documented or shipped as
a working Stage 1 provider until approved and implemented.

## 2. Background: how Otto authorizes today

Otto has one authorization path:

- `internal/config/resolve.go` rejects any provider other than
  `openai-compatible` (`resolve.go:81`) and resolves `APIKey` from environment
  variables only (`resolveAPIKey`, `resolve.go:215`).
- `cmd/otto/runtime_builder.go:159` builds the provider as
  `openaicompat.New(runtime.BaseURL, runtime.APIKey, nil)`.
- `internal/provider/openaicompat/client.go:114` sends a single static header:
  `Authorization: Bearer <apiKey>`.
- The provider speaks the **chat completions** wire format
  (`/v1/chat/completions`).
- Secrets are scrubbed from sessions and errors via `agent.NewRedactor` /
  `collectSecretValues` (`runtime_builder.go:426`), and stripped from the `bash`
  environment via `credentialEnvironmentNames` (`runtime_builder.go:397`).

There is no OAuth, no token refresh, and no on-disk credential store. The
provider interface itself is already neutral and narrow
(`provider.Provider.Complete`), and `internal/model` holds provider-neutral
message types, so a second provider can be added without touching the agent
loop.

## 3. The pivotal constraint: subscription traffic uses the Responses API

The single most important design fact, and the most common implementation
error, is the endpoint.

ChatGPT-subscription requests do **not** go to `/v1/chat/completions`. They go
to the ChatGPT backend Responses API:

- Base: `https://chatgpt.com/backend-api/codex`
- Path: `/responses` (the **Responses API**, a different request/response and
  streaming JSON shape than chat completions)
- Auth: the OAuth **access token** as `Authorization: Bearer <access_token>`,
  plus a `chatgpt-account-id` header taken from the ID-token claims.
- The backend is stateless: each request sends the full conversation history.

Consequence: a subscription cannot be reached by swapping a header on the
existing `openaicompat` client. The wire format is different. Any implementation
that wires the OAuth token into the current chat-completions provider will
authenticate correctly and then fail every completion.

A related trap: the OAuth flow also supports a token-exchange
(`urn:ietf:params:oauth:grant-type:token-exchange`, requested token
`openai-api-key`) that returns a standard API key. That key bills as **API
usage / redeemable credits**, not against the ChatGPT subscription quota, and
depends on OpenAI's promotional credit program. It does not satisfy "use my
subscription." It is out of scope for this design except as a noted alternative
(§11).

## 4. Goals

- `otto login` performs the "Sign in with ChatGPT" OAuth PKCE flow and stores
  the resulting tokens locally.
- A subscription provider sends completions to the ChatGPT Responses backend,
  authorized by the stored access token, refreshing it before expiry.
- Selecting the subscription is a normal profile/provider choice, consistent
  with existing config precedence.
- Tokens are treated as secrets: never written to session JSONL, logs, error
  text, or the `bash` environment; the credential file is `0600`.
- All new code follows the mandatory offline TDD workflow (`httptest` for the
  OAuth server and the Responses backend; no network in default tests).

## 5. Non-goals

- Anthropic / Claude subscription auth (separate future item).
- The token-exchange "API key" mode (§11 alternative only).
- Any change to Stage 1 chat-completions behavior or the `openaicompat`
  provider's wire structs.
- A hosted or multi-machine credential sync.
- Full Responses API feature parity beyond what the agent loop needs
  (text, tool calls, usage, streaming). No image input, no server-side state.

## 6. Design principles

1. **Provider-neutral loop stays neutral.** The new provider implements the
   existing `provider.Provider` interface using `internal/model` types. The
   agent loop, tools, and frontends do not learn about OAuth.
2. **Auth is separate from the provider.** The provider consumes a
   `TokenSource`; it does not know how tokens are obtained, stored, or
   refreshed. This keeps the fragile, security-sensitive OAuth code in one
   package and keeps the provider testable with a fake token source.
3. **No secret persistence beyond the credential file.** Reuse the existing
   redaction and env-stripping paths; add the token values to them.
4. **Fail clearly.** Missing/expired/unrefreshable credentials produce a typed
   error that tells the user to run `otto login`, never a silent fallback to a
   different auth mode.

## 7. Architecture

Recommended shape (Option B below):

```
internal/auth/                 (new) OAuth PKCE flow, token store, refresh, TokenSource
  oauth.go        authorize URL, PKCE verifier/challenge, code exchange
  callback.go     loopback HTTP server on :1455 (fallback :1457)
  store.go        ~/.otto/auth/chatgpt.json, 0600, load/save
  tokensource.go  TokenSource: returns a valid access token, refreshing if near expiry
  claims.go       parse chatgpt_account_id (and plan) from the ID-token JWT

internal/provider/openairesponses/   (new) ChatGPT Responses API provider
  client.go       Complete(): builds /responses request, injects Bearer + chatgpt-account-id
  protocol.go     Responses request/response/SSE wire structs (kept in this package)
  stream.go       SSE event parsing → provider.StreamEvent

cmd/otto/         login command + provider wiring
config/           allow the new provider value; resolve a TokenSource instead of an env APIKey
```

### 7.1 Why a new provider, not a proxy or a header swap

- **Option A — reuse the chat-completions provider + a translating proxy.**
  Keep `openaicompat`, run a local process that converts chat-completions
  requests to Responses API and injects OAuth headers (the `codex-proxy`
  pattern). Rejected as the primary design: it needs a background process/port,
  a bidirectional chat-completions↔Responses translator, and lifecycle
  management. That is *more* moving parts and *more* code than a provider that
  speaks Responses directly.
- **Option B — new `openairesponses` provider (recommended).** The wire format
  differs anyway, so the Responses translation has to exist somewhere. Putting
  it behind the existing `provider.Provider` interface is the smallest correct
  surface: no extra process, provider stays swappable, the agent loop is
  untouched. This matches `AGENTS.md`'s rule to keep provider-specific wire
  structs inside a provider package.

### 7.2 OAuth login flow (`otto login`)

Confirmed constants from the Codex CLI implementation (verify against
`openai/codex` `codex-rs/login` at implementation time):

- client_id: `app_EMoamEEZ73f0CkXaXp7hrann`
- authorize endpoint: `https://auth.openai.com/oauth/authorize` (PKCE, `S256`)
- token endpoint: `https://auth.openai.com/oauth/token`
- redirect_uri: `http://localhost:1455/auth/callback` (fallback port `1457`)
- scopes: `openid profile email offline_access`
- ID token JWT claim used for routing: `chatgpt_account_id`

Steps:

1. Generate PKCE verifier + `S256` challenge and a random `state`.
2. Start the loopback callback server; open the authorize URL in the browser
   (print it as a fallback for headless use).
3. On callback, validate `state`, exchange the code at the token endpoint for
   `access_token`, `refresh_token`, `id_token`, `expires_in`.
4. Parse `chatgpt_account_id` from the ID token.
5. Write `~/.otto/auth/chatgpt.json` (`0600`) with tokens, expiry, account id.
6. Print success; do not print token values.

`otto logout` deletes the credential file. `otto login --status` prints whether
credentials exist and the account id (never the tokens).

### 7.3 Token storage and refresh

- File: `~/.otto/auth/chatgpt.json`, mode `0600`, alongside the existing
  `~/.otto/` tree.
- `TokenSource.Token()` returns the current access token, transparently
  refreshing via the refresh-token grant when within ~5 minutes of expiry, and
  persisting the rotated tokens back to the file.
- Refresh failure returns a typed error instructing the user to re-run
  `otto login`.

Recommendation: use `golang.org/x/oauth2` for the authorization-code exchange,
refresh, and `TokenSource` plumbing. It is standard, vetted, and already present
transitively (via `golang.org/x/net`). This is a new *direct* dependency —
flagged as a decision in §12. Custom code still owns the loopback server, the
ChatGPT claims parsing, and the `0600` token file.

### 7.4 Config and runtime wiring

- Add a provider value (proposed: `chatgpt`) accepted by
  `config.Resolve`. When selected:
  - `base_url` and `api_key_env` are not required (the base URL is fixed to the
    ChatGPT backend; auth comes from the credential file).
  - Resolution produces a runtime that carries an auth/token source instead of
    a static `APIKey`.
- `runtime_builder.go` constructs `openairesponses.New(tokenSource)` for that
  provider, and `openaicompat.New(...)` as today for `openai-compatible`. This
  is the one branch point; everything downstream is provider-neutral.
- Precedence (flags > `OTTO_*` > TOML profile > session defaults) is unchanged;
  the subscription is just another `provider`/`profile` value.

### 7.5 Provider implementation notes

- `Complete()` serializes the full message history to a Responses request,
  sets `Authorization: Bearer <token from TokenSource>` and
  `chatgpt-account-id`, plus the originator / `OpenAI-Beta` headers the backend
  requires (exact values verified against the Codex client at implementation
  time; kept as constants in `openairesponses/protocol.go`).
- SSE streaming is mapped to `provider.StreamEvent` (`text_delta`,
  `tool_call_delta`) so both frontends render it unchanged.
- Usage is mapped to `model.Usage` for the existing context/compaction and
  footer displays.

## 8. Security

- Access, refresh, and ID tokens are added to `collectSecretValues` so the
  redactor scrubs them from session JSONL and error text, and to
  `credentialEnvironmentNames`-equivalent handling so they never enter the
  `bash` environment.
- The credential file is `0600`; the provider reads tokens through the
  `TokenSource` and never logs them.
- The OAuth `state` parameter is validated on callback; PKCE prevents code
  interception.
- Per `AGENTS.md`: no tokens in TOML, fixtures, logs, docs, or tests; sample
  values in examples are redacted.

## 9. Testing (offline, TDD)

- PKCE verifier/challenge generation: unit test the `S256` derivation.
- OAuth exchange + refresh: `httptest` server standing in for
  `auth.openai.com`; assert code exchange, near-expiry refresh, token rotation
  persisted to a `t.TempDir()` file, and typed error on refresh failure.
- Loopback callback: start the server, hit `/auth/callback` with good/bad
  `state`, assert success and rejection.
- Responses provider: `httptest` server standing in for the ChatGPT backend;
  assert request shape, injected headers, SSE → `StreamEvent` mapping, usage
  mapping, and error handling.
- Redaction: assert tokens are scrubbed from session output and errors.
- Config: assert the new provider value resolves without `base_url`/`api_key`
  and errors clearly when credentials are absent.
- Keep everything in the default offline suite; any test that would hit the
  real OpenAI endpoints is opt-in behind an env var and excluded by default.

## 10. Delivery sequence

1. [done] `internal/auth`: PKCE, token store, refresh, `TokenSource` (+ tests).
2. [done] `otto login` / `logout` / `login --status` commands wired to
   `internal/auth` (+ tests).
3. [done] `internal/provider/openairesponses`: Responses client + SSE mapping
   (+ tests).
4. [done] Config + `runtime_builder` wiring for the `chatgpt` provider
   (`config.ProviderChatGPT`; base_url/api_key skipped, model still required)
   (+ tests).
5. [done] Redaction: the provider redacts its own access token from errors
   (tested). No further wiring — OAuth tokens are file-based, not env vars, so
   bash env-stripping does not apply; the credential file is outside the
   workspace, so file tools cannot read it; and a rotated token cannot be
   pre-seeded into the once-built agent redactor, which is why redaction lives
   in the provider.
6. [deferred] Docs: `AGENTS.md`/README scope left untouched per the approval
   decision to keep Stage 1 scope declarations unchanged. `otto login`/`logout`
   are discoverable via `otto --help`.

## 11. Alternative considered: token-exchange API key

Instead of the Responses backend, exchange the OAuth ID token for an
`openai-api-key` and reuse the existing chat-completions provider against
`api.openai.com/v1`. Simpler (no new provider), but it bills as API
usage/credits rather than the ChatGPT subscription quota and depends on
OpenAI's promo program. It does not meet the stated goal. Documented here only
so the distinction is explicit.

## 12. Open questions (resolved during implementation)

1. **Provider name.** Resolved: `chatgpt` (`config.ProviderChatGPT`).
2. **`x/oauth2` dependency.** Resolved: added as a direct dependency
   (`golang.org/x/oauth2 v0.36.0`); it supplies PKCE (`GenerateVerifier`,
   `S256ChallengeOption`), code exchange, and the refreshing `TokenSource`.
3. **Provider package boundary.** Resolved: sibling
   `internal/provider/openairesponses`; no Responses shape leaks into
   `openaicompat`.
4. **Headless login.** Resolved: `otto login` both auto-opens the browser
   (`open <url>`) and always prints the authorization URL as the fallback, so a
   failed launch does not break the flow.
5. **Model list.** Resolved: no allowlist; the user supplies the model id in
   config (e.g. `gpt-5-codex`). The existing `resolveModelLimits` table already
   covers `gpt-5-codex` (`gpt400KFamily`), so no compaction changes were needed.
