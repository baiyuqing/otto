//! Decided receipts leave the active directory but remain addressable by ID.

use std::io::Read;

use super::format::{MAX_FILE_BYTES, parse_candidate};
use super::{FileStore, Snapshot, guard_candidate, publish_at, unavailable};
use crate::fsops;
use crate::memory::{Candidate, CandidateRef, CandidateState, Error, ErrorKind, Result};

const ARCHIVE: &str = "candidates/archive";

impl FileStore {
    pub(super) fn read_candidate(
        &self,
        directory: &str,
        reference: &CandidateRef,
    ) -> Result<Option<Candidate>> {
        Self::file_id(&reference.id, "candidate ID")?;
        let archived = directory == ARCHIVE;
        let directory =
            fsops::open_dir_no_follow(&self.root.join(directory)).map_err(unavailable)?;
        let file = match fsops::open_at_no_follow(
            &directory,
            &format!("{}.json", reference.id),
            libc::O_RDONLY | libc::O_NONBLOCK,
        ) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(Error::new(ErrorKind::Corrupt)),
        };
        if !file.metadata().map_err(unavailable)?.is_file() {
            return Err(Error::new(ErrorKind::Corrupt));
        }
        let mut content = String::new();
        file.take(MAX_FILE_BYTES + 1)
            .read_to_string(&mut content)
            .map_err(|_| Error::new(ErrorKind::Corrupt))?;
        if content.len() as u64 > MAX_FILE_BYTES {
            return Err(Error::new(ErrorKind::Corrupt));
        }
        let candidate =
            parse_candidate(&reference.id, &content).map_err(|_| Error::new(ErrorKind::Corrupt))?;
        guard_candidate(self.guard(), &candidate)?;
        if archived && candidate.state == CandidateState::Pending {
            return Err(Error::new(ErrorKind::Corrupt));
        }
        Ok((candidate.proposed.scope == reference.scope).then_some(candidate))
    }

    /// Caller holds the store lock. Publish the receipt before removing its
    /// active file, so interruption never loses the receipt.
    pub(super) fn archive_candidate(&self, candidate: &Candidate) -> Result<()> {
        let reference = CandidateRef {
            scope: candidate.proposed.scope.clone(),
            id: candidate.id.clone(),
        };
        if let Some(existing) = self.read_candidate(ARCHIVE, &reference)? {
            if existing != *candidate {
                return Err(Error::new(ErrorKind::Conflict));
            }
        } else {
            if std::fs::symlink_metadata(self.path(ARCHIVE, &candidate.id, "json")).is_ok() {
                return Err(Error::new(ErrorKind::Conflict));
            }
            publish_at(
                &self.root,
                ARCHIVE,
                &candidate.id,
                "json",
                &super::format::encode_candidate_file(candidate)?,
            )?;
        }
        let directory =
            fsops::open_dir_no_follow(&self.root.join("candidates")).map_err(unavailable)?;
        match fsops::unlink_at(&directory, &format!("{}.json", candidate.id)) {
            Ok(()) => fsops::sync_dir(&directory).map_err(|_| Error::new(ErrorKind::CommitUnknown)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(Error::new(ErrorKind::CommitUnknown)),
        }
    }

    pub(super) fn archive_decided_candidates(&self, snapshot: &Snapshot) -> Result<()> {
        for candidate in &snapshot.candidates {
            if candidate.state != CandidateState::Pending {
                self.archive_candidate(candidate)?;
            }
        }
        Ok(())
    }

    /// Only history listings scan archived receipts. Prefer an active copy if
    /// a previous archive was interrupted between publication and removal.
    pub(super) fn include_candidate_archive(&self, snapshot: &mut Snapshot) -> Result<()> {
        let archived = self.read_entries(
            ARCHIVE,
            "json",
            &|id, content| {
                let candidate = parse_candidate(id, content)?;
                if candidate.state == CandidateState::Pending {
                    return Err("pending candidate in archive".into());
                }
                guard_candidate(self.guard(), &candidate).map_err(|error| error.to_string())?;
                Ok(candidate)
            },
            &mut snapshot.invalid,
        )?;
        for candidate in archived {
            if !snapshot
                .candidates
                .iter()
                .any(|active| active.id == candidate.id)
            {
                snapshot.candidates.push(candidate);
            }
        }
        Ok(())
    }
}
