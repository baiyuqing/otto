//! The ChatGPT backend Responses API wire codec.
//!
//! Port of the wire half of `internal/provider/openairesponses`: request
//! translation ([`protocol`]) and the server-sent-event response assembler
//! ([`stream`]). The HTTP half of that Go package, the client, its timeouts,
//! its redirect policy, and its credential redaction, is not here. It lives in
//! `otto::provider::chatgpt`, which drives these two modules.
//!
//! There is no overflow classifier here, unlike [`crate::openaicompat`]. The
//! Go client for this backend reads no error body and classifies no status: a
//! non-2xx response becomes `chatgpt responses HTTP {status}` and nothing
//! else, so there is nothing to classify.
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

pub mod protocol;
pub mod stream;
