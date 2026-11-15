//! Process-wide primitives of Otto with no domain knowledge: the pinned POSIX
//! file driver, logging, retry and deadline helpers, TLS provider selection,
//! URL parsing and redaction, child-process tracking and process identity.
//! They sit between `otto-core` and `otto`.

pub mod child_fence;
pub mod deadline;
pub mod fsops;
mod gourl;
pub mod herdr;
pub mod hex;
pub mod log;
pub mod process_identity;
pub mod program_status;
pub mod random;
pub mod retry;
pub mod tls_provider;
pub mod urlprivacy;
