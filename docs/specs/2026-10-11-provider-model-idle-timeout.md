# Provider model-output idle timeout

Status: approved 2026-10-11. Issue
[#470](https://github.com/baiyuqing/otto/issues/470).

Decision 2026-10-11: only model output resets the timer. Model output is a
text, reasoning or tool-argument delta produced by the stream assembler.
Other server events, SSE comments and bytes that parse into nothing do not
reset it.

## Current behavior

- Both providers bound a response body only with reqwest's per-read timeout:
  60 s (`chatgpt.rs` `READ_TIMEOUT`, `openaicompat.rs` `READ_TIMEOUT`). Every
  received chunk restarts it. Neither client sets an overall request timeout.
- A stream that keeps delivering bytes but produces no model deltas is read
  until the user cancels, the turn deadline expires, or the server closes it.
  Interactive turns have no deadline.
- Observed on `chatgpt` / `gpt-5.6-sol`:
  - 2026-10-11, operation `op_b52c378b…`: 87 deltas (321 bytes) between
    8.06 s and 9.39 s, then 816 bytes over 178 s with no delta, until the user
    cancelled at 199.7 s.
  - 2026-10-09, operation `op_4de3382f…`: no delta from 1.5 s to 815.8 s while
    bytes kept arriving; the request then ended with a provider error.
  - In both, `network_bytes` grew by +42 / +94 bytes per 15 s heartbeat, which
    suggests a fixed-interval server keepalive. The body is not logged, so
    this is not verified.

## Threshold data

Source: every local diagnostic log, 1082 `chatgpt` / `gpt-5.6-sol` provider
operations that succeeded on their first attempt (2026-10-09 to 2026-10-11).
Longest silence per operation = time from the first body byte to the first
delta, and `elapsed_ms - last_model_ms` at each 15 s heartbeat.

| Percentile | Longest silence |
| --- | --- |
| p50 | 2.4 s |
| p90 | 9.4 s |
| p99 | 21.8 s |
| max | 57.9 s |

Heartbeats are 15 s apart, so a sampled silence understates the real one by
less than 15 s; the largest real silence in this set is below 73 s. No data
exists for `openai-compatible` endpoints.

## Design

### Timer

Each provider attempt keeps one deadline, `last_progress + bound`:

- It starts when a 2xx response head is received. Before that, the existing
  connect and per-read timeouts apply.
- It restarts when `assembler.push` or `assembler.finish` emits at least one
  `StreamEvent` for the chunk. Both assemblers emit only `TextDelta`,
  `ReasoningDelta` and `ToolCallDelta`, and the `openai-compatible` assembler
  already drops empty deltas. The check sits on the assembler callback, before
  the ChatGPT `EventRedactor`, so text the redactor holds back still counts.
- Each body read becomes
  `await_control(control, tokio::time::timeout_at(deadline, body.try_next()))`.
  `tokio::time::Timeout` polls the read before the timer, so a chunk that is
  ready at expiry is still processed.
- A retry starts a new attempt and a new timer. The turn deadline and user
  cancellation keep precedence through `await_control`.

### Bounds

| Provider | Bound | Reason |
| --- | --- | --- |
| `chatgpt` | 120 s | 2.1x the largest sampled silence (57.9 s), 1.6x its upper bound (73 s) |
| `openai-compatible` | 300 s | No data. Endpoints and proxies that hide reasoning may send only SSE comments while the model reasons. An endpoint that sends no bytes at all is already stopped by the 60 s read timeout. |

Both are constants beside `READ_TIMEOUT`. There is no configuration key.

Applied to the observed incidents, the `chatgpt` bound would have ended the
2026-10-11 attempt at 129.4 s instead of 199.7 s, and the 2026-10-09 attempt
at 121.5 s instead of 815.8 s.

### On expiry

- `chatgpt`: the attempt fails with retry reason `model output idle timeout`,
  `EffectCertainty::Unknown`, and goes through the existing retry loop (up to
  six retries, 1/2/4/8/16/30 s, `interrupted_output` set when the attempt
  emitted a delta). After the last retry the error reads
  `chatgpt request failed (model output idle timeout; 7 attempts; retry budget exhausted)`.
- `openai-compatible`: before any delta reached the caller, the attempt is
  `AttemptError::Interrupted` and is retried under the existing policy
  (4 attempts, 1/2/4 s). `Interrupted` gains a reason field so the `Retry`
  event reads `model output idle timeout` instead of `connection interrupted`.
  After a delta reached the caller, the attempt is `AttemptError::Sent` with
  `read chat completion stream: no model output for 300s` and is not
  retried, matching the existing rule that visible output is never replayed.
  The `read chat completion stream:` prefix makes the settlement
  `transport_lost` with `Unknown` certainty, like other body read failures.
- `otto_core::diagnostics` maps the reason `model output idle timeout` to
  `RetryReason::Timeout`.

### Test injection

`chatgpt::Client` and `openaicompat::Client` gain
`with_model_idle_timeout(self, Duration) -> Self`, used by tests the same way
`with_http_client` injects a 200 ms read timeout today.

## Not covered

- A stream that keeps producing deltas and never completes. Output-token
  limits bound it.
- The 2026-10-09 failure after 815.8 s: 34 MB and 127,950 deltas in 5.3 s,
  then a provider error. Separate defect if it recurs.
- A configuration key for the bounds. Add one when a user reports a false
  timeout.
- Mapping the existing `stream read timeout` reason to `RetryReason::Timeout`;
  it stays `Other`.

## Tests

Rows in `crates/otto/tests/fault_provider.rs`, using `faults::http::Script`
with `Step::Chunk(b": keepalive\n\n")` every 50 ms, a 200 ms read timeout and
a 500 ms model idle bound. Each test is wrapped in a 10 s
`tokio::time::timeout`, so without the change it fails instead of hanging.

| Test | Assertion that fails without the change |
| --- | --- |
| `chatgpt_keepalive_without_model_output_is_retried` | one retry with reason `model output idle timeout`, `interrupted_output: false`, second attempt succeeds, `server.count() == 2` |
| `chatgpt_keepalive_after_one_delta_is_retried` | same, with `interrupted_output: true` |
| `chatgpt_spaced_deltas_do_not_time_out` | deltas every 300 ms for 1.5 s, interleaved with keepalives: no retry, one request |
| `compat_keepalive_before_output_is_retried` | one retry with reason `model output idle timeout`, second attempt succeeds |
| `compat_keepalive_after_output_fails_without_retry` | error contains `no model output`, certainty `Unknown`, `server.count() == 1` |

## Documentation

- `docs/manual/getting-started.md`: provider retry paragraph gains the
  model-output idle bound for both providers.
- `docs/manual/servers-and-connectors.md`: `provider_retry.reason` examples
  gain `model output idle timeout`.
- `docs/development/architecture.md`: provider recovery section.
- The `READ_TIMEOUT` comments in both providers stop claiming they bound a
  stalled body.
- `CHANGELOG.md` entry under Fixed.
