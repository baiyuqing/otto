//! Profile changes retain typed operations and hashes, not whole config copies.
use super::{Domain, Error, Result};
use crate::config::{ProfileChange, ProfileChangePreview};
use crate::hex::to_hex;
use otto_core::approval::{Request, State};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub change: ProfileChange,
    pub before: String,
    pub after: String,
    pub diff: String,
}
impl Artifact {
    pub fn authority(&self) -> otto_core::approval::Authority {
        use crate::config::{ProfileChange, ProfileField};
        if matches!(
            &self.change,
            ProfileChange::Create { .. }
                | ProfileChange::SetField {
                    field: ProfileField::BaseUrl | ProfileField::ApiKeyEnv,
                    ..
                }
        ) {
            otto_core::approval::Authority::Consent
        } else {
            otto_core::approval::Authority::Durable
        }
    }
    pub fn prepare(change: ProfileChange, preview: &ProfileChangePreview) -> Self {
        Self {
            change,
            before: hash(&preview.original),
            after: hash(&preview.updated),
            diff: preview.redacted_diff.clone(),
        }
    }
}
fn hash(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    to_hex(&Sha256::digest(bytes))
}
pub struct Adapter {
    pub path: PathBuf,
}
impl Adapter {
    fn artifact(&self, request: &Request) -> Result<Artifact> {
        if super::digest(&request.subject.details) != request.subject.version {
            return Err(Error::Conflict);
        }
        serde_json::from_str(&request.subject.details).map_err(|_| Error::Unavailable)
    }
}
impl Domain for Adapter {
    fn details(&self, request: &Request) -> Result<String> {
        let artifact = self.artifact(request)?;
        Ok(format!(
            "{}\n\nOperation: {}\nRestart Otto after saving to apply profile changes.",
            artifact.diff,
            serde_json::to_string_pretty(&artifact.change).map_err(|_| Error::Unavailable)?
        ))
    }
    fn apply(&self, request: &Request) -> Result<()> {
        if request.state != State::Approved {
            return Ok(());
        }
        let artifact = self.artifact(request)?;
        let current = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
            Err(_) => return Err(Error::Unavailable),
        };
        if hash(&current) == artifact.after {
            return Ok(());
        }
        if hash(&current) != artifact.before {
            return Err(Error::Conflict);
        }
        let preview = crate::config::preview_profile_change(&self.path, &artifact.change)
            .map_err(|_| Error::Conflict)?;
        if hash(&preview.original) != artifact.before || hash(&preview.updated) != artifact.after {
            return Err(Error::Conflict);
        }
        crate::config::commit_profile_change(&self.path, &preview).map_err(|_| Error::Conflict)
    }
}
