//! The OpenAI-compatible Chat Completions wire codec.
//!
//! Port of the wire half of `internal/provider/openaicompat`: request
//! translation ([`protocol`]), the server-sent-event response assembler
//! ([`stream`]), and the context-overflow classifier ([`overflow`]). The HTTP
//! half of that Go package, the client, its timeouts, its retry policy, and
//! its API-key redaction, is not here; it is phase 4 of the rewrite and lives
//! in `crates/otto`, which will drive these three modules.
//!
//! Ownership: nothing in this module holds shared or global state. Free
//! functions borrow their input and return owned values;
//! [`stream::StreamAssembler`] owns the partial state of exactly one response
//! and is moved, not shared.
//!
//! Concurrency: there is no interior mutability and no static mutable state,
//! so separate calls and separate assemblers never interact. Two responses
//! decode concurrently by using two assemblers.
//!
//! Errors: [`stream::StreamError`] covers every rejected response body, with
//! the same message text the Go implementation produces.
//! [`overflow::classify_overflow`] reports "not an overflow" as `None` rather
//! than as an error, because an unclassified error body is still an error, it
//! just is not a context-window rejection.

pub mod overflow;
pub mod protocol;
pub mod stream;
