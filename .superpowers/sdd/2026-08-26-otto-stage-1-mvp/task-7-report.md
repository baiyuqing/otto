# Task 7 Implementation Report

## Status

COMPLETE

Commit: `ea4f6c2d52fa131c66bbfedf4ccc05376f9d2da0` (`feat: add OpenAI-compatible streaming`)

## Implemented

- Added `internal/provider/openaicompat` as the sole owner of OpenAI Chat Completions wire and HTTP/SSE types.
- Added neutral-to-OpenAI request translation for system, user, assistant/tool-call, tool-result, and tool-schema messages.
- Preserved base URL paths and appended `/chat/completions` after URL validation/normalization.
- Added streaming Chat Completions requests with bearer/JSON/SSE headers, `stream: true`, and usage streaming enabled.
- Added `bufio.Reader.ReadString('\n')` SSE parsing with multiline `data:`, CRLF, comments, ignored non-data fields, events larger than 64 KiB, usage-only chunks, and mandatory `[DONE]` handling.
- Added first-seen-order indexed tool-call assembly, fragmented argument streaming, final JSON validation, text/tool delta callbacks, finish-reason mapping, and cancellation propagation.
- Added at most three total attempts for connection/read failures before visible deltas, HTTP 429, and HTTP 5xx. Added integer/date `Retry-After`, 250 ms/500 ms defaults, injectable sleep, and no retry after text/tool deltas or redirect-policy/non-retryable errors.
- Added 32 KiB error-body limits, API-key/Authorization redaction, and response-body closure on success and failure paths.

## TDD Evidence

Focused RED/GREEN cycles included:

1. RED: package test failed with `undefined: New`; GREEN: primary text/tool-fragment/usage stream passed.
2. RED: empty tool schema omitted required `description`; GREEN: exact function schema fields were emitted.
3. RED: 429/503 tests returned immediately instead of retrying; GREEN: three-attempt bound and 250/500 ms delays passed.
4. RED: redirect-policy error was retried three times; GREEN: only nil-response transport/connection failures are retryable.

Additional integration coverage includes request translation, multiple indexed calls, CRLF/comments/multiline SSE, >64 KiB events, malformed JSON/arguments, missing `[DONE]`, unknown finish reasons, cancellation, connection and stream-read retries, post-delta retry suppression, `Retry-After: 0`, HTTP-date retry, unauthorized redaction/body bounds, invalid base URLs, and body closure.

## Verification

Fresh verification after the final production change:

- `go test ./internal/provider/openaicompat -count=1` — PASS
- `go test -race ./internal/provider/openaicompat -count=1` — PASS
- `go test ./... -count=1` — PASS
- `go test -race ./... -count=1` — PASS
- `gofmt -d internal/provider/openaicompat` — clean
- `go vet ./...` — PASS
- `git diff --check` — clean

## Self-Review

- Reviewed the complete staged diff against every Task 7 requirement.
- Confirmed no provider-specific wire types leaked outside `internal/provider/openaicompat`.
- Found and fixed one over-broad retry case during review: `http.Client` redirect-policy errors with a non-nil response are now non-retryable, with a RED/GREEN regression test.
- Confirmed all response paths close bodies and all externally formatted provider errors pass through redaction.
- No Critical or Important findings remain. A separate reviewer subagent was unavailable in this Pi session, so the requested self-review was performed directly.

## Concerns

No blocking concerns. Because the approved `New(baseURL, apiKey, httpClient)` call shape has no error return, invalid base URLs are retained as constructor state and reported by `Complete` before any HTTP request.

## Fix Round 1

### Status

COMPLETE

### Changes

- Preserved `context.Canceled` / `context.DeadlineExceeded` identity when retry backoff sleep is interrupted by returning `ctx.Err()` directly and leaving context errors untouched in redaction.
- Changed tool-call continuation stream events to emit the assembled call ID/name for every delta, so interleaved continuations remain attributable even when the raw chunk omits those fields.
- Added focused regression coverage for both findings.

### TDD Evidence

RED:

```text
$ go test ./internal/provider/openaicompat -run 'TestCompletePreservesCancellationIdentityDuringRetryBackoff|TestCompleteEmitsStableToolCallIdentityForInterleavedContinuations' -count=1
--- FAIL: TestCompletePreservesCancellationIdentityDuringRetryBackoff (0.00s)
    client_test.go:239: error = context canceled, want context cancellation identity
--- FAIL: TestCompleteEmitsStableToolCallIdentityForInterleavedContinuations (0.00s)
    stream_test.go:138: event[2] = provider.StreamEvent{Type:"tool_call_delta", Text:"", ToolCallID:"", ToolName:"", Arguments:"th\":\"A\"}"}, want provider.StreamEvent{Type:"tool_call_delta", Text:"", ToolCallID:"call-a", ToolName:"read", Arguments:"th\":\"A\"}"}
FAIL
FAIL	github.com/baiyuqing/otto/internal/provider/openaicompat	0.469s
FAIL
```

GREEN:

```text
$ go test ./internal/provider/openaicompat -run 'TestCompletePreservesCancellationIdentityDuringRetryBackoff|TestCompleteEmitsStableToolCallIdentityForInterleavedContinuations' -count=1
ok  	github.com/baiyuqing/otto/internal/provider/openaicompat	0.386s
```

### Verification

```text
$ gofmt -w internal/provider/openaicompat/client.go internal/provider/openaicompat/client_test.go internal/provider/openaicompat/stream.go internal/provider/openaicompat/stream_test.go

$ go test ./internal/provider/openaicompat -count=1
ok  	github.com/baiyuqing/otto/internal/provider/openaicompat	0.182s

$ go test -race ./internal/provider/openaicompat -count=1
ok  	github.com/baiyuqing/otto/internal/provider/openaicompat	1.447s

$ go test ./... -count=1
ok  	github.com/baiyuqing/otto/internal/config	0.323s
ok  	github.com/baiyuqing/otto/internal/model	0.310s
?   	github.com/baiyuqing/otto/internal/provider	[no test files]
ok  	github.com/baiyuqing/otto/internal/provider/openaicompat	0.171s
ok  	github.com/baiyuqing/otto/internal/session	0.368s
ok  	github.com/baiyuqing/otto/internal/tool	2.913s

$ go test -race ./... -count=1
ok  	github.com/baiyuqing/otto/internal/config	1.321s
ok  	github.com/baiyuqing/otto/internal/model	1.317s
?   	github.com/baiyuqing/otto/internal/provider	[no test files]
ok  	github.com/baiyuqing/otto/internal/provider/openaicompat	1.212s
ok  	github.com/baiyuqing/otto/internal/session	1.384s
ok  	github.com/baiyuqing/otto/internal/tool	3.933s

$ git diff --check
```

### Self-Review

- Verified the backoff fix is limited to interrupted retry sleep; existing retry and redaction behavior remains unchanged for non-context errors.
- Verified continuation callbacks now use assembled per-index state, so later deltas inherit the original ID/name without affecting final response assembly order.
- Confirmed the new regression test uses interleaved indexes, which would have been impossible to attribute correctly from the raw chunk alone.
- No additional concerns found.
