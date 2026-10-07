//! Version-one logical records. Schemas belong to domains, physical mappings to adapters.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{Error, Record, Result};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum IdentityPart {
    Integer(i64),
    Text(String),
}

/// Version-one canonical order is part of the portable encoding, not SQL collation.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RecordOrder {
    pub kind: usize,
    pub identity: Vec<IdentityPart>,
}

pub fn order(domain: &str, record: &Record) -> Result<RecordOrder> {
    if domain == "approvals" {
        use crate::approval::migration::ApprovalRecord;
        let value: ApprovalRecord = serde_json::from_slice(&record.payload)
            .map_err(|_| Error::Invalid("invalid approval record encoding".into()))?;
        let metadata = value.metadata()?;
        if metadata != (record.id.clone(), record.revision.clone()) {
            return Err(Error::Invalid("approval record identity differs".into()));
        }
        use IdentityPart::{Integer, Text};
        let (kind, identity) = match value {
            ApprovalRecord::ManagedPolicy { workspace, .. } => (7, vec![Text(workspace)]),
            ApprovalRecord::Settings {
                workspace,
                session_id,
                ..
            } => (0, vec![Text(workspace), Text(session_id)]),
            ApprovalRecord::ReviewContext {
                workspace,
                session_id,
                ..
            } => (5, vec![Text(workspace), Text(session_id)]),
            ApprovalRecord::ReviewDenials {
                workspace,
                session_id,
                task_id,
                run_id,
                evidence_digest,
                ..
            } => (
                6,
                vec![
                    Text(workspace),
                    Text(session_id),
                    Text(task_id),
                    Text(run_id),
                    Text(evidence_digest),
                ],
            ),
            ApprovalRecord::Mode {
                workspace,
                session_id,
                ..
            } => (0, vec![Text(workspace), Text(session_id)]),
            ApprovalRecord::Request { request, .. } => (
                1,
                vec![Text(
                    crate::approval::codec::decode_persisted(&request)
                        .map_err(|_| Error::Invalid("invalid approval request".into()))?
                        .id,
                )],
            ),
            ApprovalRecord::Visibility {
                approval_id,
                workspace,
                session_id,
            } => (
                2,
                vec![Text(approval_id), Text(workspace), Text(session_id)],
            ),
            ApprovalRecord::InterruptedClaim { approval_id, .. } => (3, vec![Text(approval_id)]),
            ApprovalRecord::Event { sequence, .. } => (4, vec![Integer(sequence)]),
        };
        return Ok(RecordOrder { kind, identity });
    }
    let schemas = super::references::schemas(domain)?;
    let data = LogicalRecord::decode(record, schemas)?;
    let kind = schemas
        .iter()
        .position(|schema| schema.name == data.kind)
        .ok_or_else(|| Error::Invalid("unsupported logical record kind".into()))?;
    let identity = schemas[kind]
        .keys
        .iter()
        .map(|key| {
            let value = &data.fields[*key];
            if let Some(value) = value.as_i64() {
                Ok(IdentityPart::Integer(value))
            } else if let Some(value) = value.as_str() {
                Ok(IdentityPart::Text(value.into()))
            } else {
                Err(Error::Invalid("invalid logical identity field".into()))
            }
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(RecordOrder { kind, identity })
}

pub fn canonical_records(
    domain: &str,
    records: impl IntoIterator<Item = Record>,
) -> Result<Vec<Record>> {
    let mut ordered = records
        .into_iter()
        .map(|record| Ok((order(domain, &record)?, record)))
        .collect::<Result<Vec<_>>>()?;
    ordered.sort_by(|left, right| left.0.cmp(&right.0));
    if ordered.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(Error::Invalid("duplicate logical identity".into()));
    }
    Ok(ordered.into_iter().map(|(_, record)| record).collect())
}

pub fn digest(
    domain: &str,
    records: impl IntoIterator<Item = Record>,
) -> Result<super::DomainDigest> {
    let records = canonical_records(domain, records)?;
    let mut hash = Sha256::new();
    for record in &records {
        let bytes = serde_json::to_vec(record)
            .map_err(|_| Error::Invalid("logical digest encoding failed".into()))?;
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    }
    Ok(super::DomainDigest {
        records: records.len() as u64,
        blob_bytes: 0,
        sha256: format!("{:x}", hash.finalize()),
    })
}

#[derive(Clone, Debug)]
pub struct RecordSchema {
    pub name: &'static str,
    pub fields: &'static [&'static str],
    pub keys: &'static [&'static str],
    pub revision: Option<&'static str>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogicalRecord {
    pub version: u32,
    pub kind: String,
    pub fields: BTreeMap<String, serde_json::Value>,
}

impl LogicalRecord {
    pub fn metadata(&self, schema: &RecordSchema) -> Result<(String, String)> {
        if self.version != 1
            || self.kind != schema.name
            || self.fields.len() != schema.fields.len()
            || schema
                .fields
                .iter()
                .any(|field| !self.fields.contains_key(*field))
            || self
                .fields
                .values()
                .any(|value| !value.is_null() && !value.is_string() && !value.is_number())
        {
            return Err(Error::Invalid(
                "incompatible logical record fields or version".into(),
            ));
        }
        let keys = schema
            .keys
            .iter()
            .map(|key| {
                self.fields
                    .get(*key)
                    .cloned()
                    .ok_or_else(|| Error::Invalid("missing logical identity field".into()))
            })
            .collect::<Result<Vec<_>>>()?;
        let revision = schema
            .revision
            .and_then(|field| self.fields.get(field))
            .filter(|value| !value.is_null())
            .map(serde_json::Value::to_string)
            .unwrap_or_else(|| "1".into());
        Ok((
            format!(
                "{}:{}",
                schema.name,
                serde_json::to_string(&keys)
                    .map_err(|_| Error::Invalid("logical identity encoding failed".into()))?
            ),
            revision,
        ))
    }

    pub fn decode(record: &Record, schemas: &[RecordSchema]) -> Result<Self> {
        let value: Self = serde_json::from_slice(&record.payload)
            .map_err(|_| Error::Invalid("invalid logical record encoding".into()))?;
        let schema = schemas
            .iter()
            .find(|schema| schema.name == value.kind)
            .ok_or_else(|| Error::Invalid("unsupported logical record kind".into()))?;
        let (id, revision) = value.metadata(schema)?;
        if record.id != id || record.revision != revision {
            return Err(Error::Invalid(
                "logical identity or revision mismatch".into(),
            ));
        }
        Ok(value)
    }
}
