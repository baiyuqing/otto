//! The per-server client: era negotiation, `tools/list`, `tools/call`.
//!
//! Owned by the stdio/client step; see `docs/specs/2026-09-19-mcp-design.md`.

/// One connected server. Placeholder until the client step lands.
pub struct Client {
    _private: (),
}

impl Client {
    /// Shuts the server down. Idempotent.
    pub async fn close(&self) {}
}
