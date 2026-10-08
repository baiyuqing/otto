//! Provider-managed compaction checkpoints stored as opaque custom records.
//!
//! The encrypted content is intentionally never included in `Debug` or error
//! text. This module only validates and projects the envelope; it never
//! interprets the provider-owned blob.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::PiError;

/// Pi `customType` used for provider-managed compaction checkpoints.
pub const REMOTE_COMPACTION_CUSTOM_TYPE: &str = "otto.remote_compaction";
/// Current wire schema for [`RemoteCompaction`].
pub const REMOTE_COMPACTION_SCHEMA_VERSION: u32 = 1;
/// Largest accepted opaque encrypted blob, measured as UTF-8 bytes.
pub const MAX_REMOTE_COMPACTION_CONTENT_BYTES: usize = 8 << 20;
/// Largest accepted provider identifier, measured as UTF-8 bytes.
pub const MAX_REMOTE_COMPACTION_PROVIDER_BYTES: usize = 128;
/// Largest accepted provider item identifier, measured as UTF-8 bytes.
pub const MAX_REMOTE_COMPACTION_ID_BYTES: usize = 1024;

/// An opaque provider-managed compaction item.
#[derive(Clone, PartialEq, Eq)]
pub struct RemoteCompaction {
    pub provider: String,
    pub id: String,
    pub encrypted_content: String,
}

impl fmt::Debug for RemoteCompaction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteCompaction")
            .field("provider", &self.provider)
            .field("id", &self.id)
            .field("encrypted_content", &"[REDACTED]")
            .finish()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RemoteCompactionRecord {
    schema_version: u32,
    provider: String,
    id: String,
    encrypted_content: String,
}

/// Validates the public domain representation without inspecting its opaque
/// encrypted content.
pub fn validate_remote_compaction(value: &RemoteCompaction) -> Result<(), PiError> {
    validate_identifier(
        "remote compaction provider",
        &value.provider,
        MAX_REMOTE_COMPACTION_PROVIDER_BYTES,
    )?;
    if !value.id.is_empty() {
        validate_identifier(
            "remote compaction id",
            &value.id,
            MAX_REMOTE_COMPACTION_ID_BYTES,
        )?;
    }
    if value.encrypted_content.is_empty() {
        return Err(PiError::invalid(
            "remote compaction encrypted content must not be empty",
        ));
    }
    if value.encrypted_content.len() > MAX_REMOTE_COMPACTION_CONTENT_BYTES {
        return Err(PiError::invalid(
            "remote compaction encrypted content exceeds 8 MiB",
        ));
    }
    Ok(())
}

fn validate_identifier(name: &str, value: &str, maximum: usize) -> Result<(), PiError> {
    if value.is_empty() || value.len() > maximum {
        return Err(PiError::invalid(format!(
            "{name} must be between 1 and {maximum} bytes"
        )));
    }
    if value.trim() != value || value.chars().any(char::is_control) {
        return Err(PiError::invalid(format!(
            "{name} must not contain surrounding whitespace or control characters"
        )));
    }
    Ok(())
}

/// Encodes a validated checkpoint as the JSON value carried by a Pi custom
/// entry.
pub fn encode_remote_compaction(value: &RemoteCompaction) -> Result<String, PiError> {
    validate_remote_compaction(value)?;
    serde_json::to_string(&RemoteCompactionRecord {
        schema_version: REMOTE_COMPACTION_SCHEMA_VERSION,
        provider: value.provider.clone(),
        id: value.id.clone(),
        encrypted_content: value.encrypted_content.clone(),
    })
    .map_err(|error| PiError::new(super::PiErrorKind::Other, error.to_string()))
}

/// Decodes and validates schema version 1. Unknown versions and malformed
/// records are rejected without including encrypted content in the error.
pub fn decode_remote_compaction(data: &str) -> Result<RemoteCompaction, PiError> {
    let record: RemoteCompactionRecord = serde_json::from_str(data)
        .map_err(|_| PiError::invalid("malformed remote compaction record"))?;
    if record.schema_version != REMOTE_COMPACTION_SCHEMA_VERSION {
        return Err(PiError::invalid(
            "unsupported remote compaction schema version",
        ));
    }
    let value = RemoteCompaction {
        provider: record.provider,
        id: record.id,
        encrypted_content: record.encrypted_content,
    };
    validate_remote_compaction(&value)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkpoint() -> RemoteCompaction {
        RemoteCompaction {
            provider: "openai-compatible".into(),
            id: "item_123".into(),
            encrypted_content: "opaque-secret".into(),
        }
    }

    #[test]
    fn round_trips_camel_case_wire_record() {
        let encoded = encode_remote_compaction(&checkpoint()).expect("encode");
        assert_eq!(
            encoded,
            r#"{"schemaVersion":1,"provider":"openai-compatible","id":"item_123","encryptedContent":"opaque-secret"}"#
        );
        assert_eq!(
            decode_remote_compaction(&encoded).expect("decode"),
            checkpoint()
        );
    }

    #[test]
    fn rejects_invalid_and_oversize_values_without_echoing_content() {
        for value in [
            RemoteCompaction {
                provider: "".into(),
                ..checkpoint()
            },
            RemoteCompaction {
                id: " bad".into(),
                ..checkpoint()
            },
            RemoteCompaction {
                encrypted_content: "x".repeat(MAX_REMOTE_COMPACTION_CONTENT_BYTES + 1),
                ..checkpoint()
            },
        ] {
            let error = encode_remote_compaction(&value).expect_err("invalid accepted");
            assert!(!error.to_string().contains(&value.encrypted_content));
        }
        assert!(!format!("{:?}", checkpoint()).contains("opaque-secret"));
    }

    #[test]
    fn rejects_unknown_schema_and_fields() {
        assert!(
            decode_remote_compaction(
                r#"{"schemaVersion":2,"provider":"p","id":"i","encryptedContent":"x"}"#
            )
            .is_err()
        );
        assert!(
            decode_remote_compaction(
                r#"{"schemaVersion":1,"provider":"p","id":"i","encryptedContent":"x","extra":true}"#
            )
            .is_err()
        );
    }
}
