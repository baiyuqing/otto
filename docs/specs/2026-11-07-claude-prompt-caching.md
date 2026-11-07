# Claude prompt caching and complete input accounting

Status: approved by the user on 2026-11-07; implemented.

## Goal and acceptance

Reduce repeated Claude prompt charges without losing conversation information.
Correctness, measurable accounting and compatibility take priority over diff size.
The motivating session accumulated 8,982,448 reported input tokens in 88 successful
calls by 2026-11-07 10:29:30 Asia/Shanghai. This is recorded usage, not an invoice.

The change must emit cache controls by default, keep repeated request prefixes
stable, and preserve separately reported cache reads and writes through response,
session recording, durable usage storage and aggregate queries. Missing usage
breakdowns stay unknown; explicit zero stays known. Offline tests establish the
wire contract and accounting. They do not establish a real endpoint's hit rate.

## Request strategy

- Use documented automatic caching with top-level
  `cache_control: {"type":"ephemeral"}` and the default five-minute lifetime.
- Also anchor the last tool definition and the system text with explicit
  five-minute breakpoints. This retains reusable tools when the system changes
  and the system prefix when the moving conversation breakpoint cannot match.
  Together with automatic conversation caching there are at most three points,
  below the API limit of four. Omit anchors for absent tools/system.
- All calls, child calls, compaction calls, retries and payload-size estimates
  share the same encoder. Retry bodies remain semantically stable. Do not strip
  cache controls and silently resend an API-rejected request at full input price.
- Use no local prompt cache, invented session cache key, provider switch or
  hardcoded model-pricing table. Keep model and message content unchanged.
- Five minutes avoids the higher one-hour write premium for ordinary active
  coding loops. Expiry or an interrupted prefix can still produce a paid write;
  document this rather than promise a session-wide savings percentage.

## Complete usage contract

- Normalize total input to uncached input + cache-read input + cache-write input.
  Keep existing `cached_input_tokens` as cache reads.
- Extend neutral Usage with optional `cache_write_input_tokens` and
  `cache_write_1h_input_tokens`. The latter is a subset of the former. Optional
  values preserve missing versus explicit zero without guessing from the request.
- Maintain separate cumulative SSE counters. Partial/null updates preserve prior
  values; repeated updates replace rather than add totals. Validate integers,
  nonnegativity, checked sums, nested five-minute/one-hour consistency, and
  read+write <= total input. Unknown future usage fields remain ignorable.
- Carry write counts through provider events, wire serialization, usage.db,
  portable storage migration and summary/daily queries. Aggregate an optional
  counter only when every reported usage contributing to it has that breakdown.
- Reuse Pi `cacheWrite`/`cacheWrite1h`; preserve explicit-zero presence in Otto
  details where Pi's mandatory zero cannot express unknown. Existing sessions
  remain readable and append-only; old database rows receive nullable fields,
  without fabricated backfills. Portable usage schema v3 still imports v2 and
  verifies its original digest with null write counters, never invented zeroes.
  Other providers retain their current counters.
- Preserve the new counters in compaction and session usage accumulation.
  Existing TUI input/cache-hit totals use normalized values. Write breakdowns
  remain available to API/storage consumers; no amount is presented as a bill.

## Context management

Automatic compaction already defaults on and follows catalog/model overrides.
Opus's one-million-token window explains why the observed session was not yet
compacted. Keep those correctness-oriented limits in this change. Lowering them
solely to save money would replace useful context with a lossy summary and can
invalidate the prompt cache. Document existing explicit compaction controls;
evaluate a different default separately with task-quality and cost evidence.

## Verification and delivery

- Offline codec tests cover anchors, unchanged messages/tools, stable repeated
  encoding, empty sections, cumulative/partial SSE, read/write/TTL breakdowns,
  explicit zero, missing fields, malformed values and overflow.
- A scripted native Claude exchange covers first-write then cache-hit responses,
  normalized provider events, session round trips and persisted aggregates.
- Storage tests cover additive legacy upgrade, unknown fields, explicit-zero
  totals, mixed completeness, and portable migration. Compaction/session tests
  cover accumulation without loss or invented counts.
- Run formatting, focused tests, core WASM checks and code limits; update the
  canonical manual, usage contract and changelog; then commit, push and open PR.
- No real provider calls, user-store migrations or deployment during verification.
  Failed calls lacking usage remain a stated accounting limitation.

Reference: [Anthropic prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching).
