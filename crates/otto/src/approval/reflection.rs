//! Durable skill review artifacts. The guard still constructs every Vetted;
//! only reflection::skillwrite mutates skill files.
use super::{Authority, Domain, Error, Kind, Proposal, Request, Result, Service, State, Subject};
use crate::reflection::{guard, repository::ReflectionStore, skillwrite, transcript};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Artifact {
    candidate: guard::Candidate,
    reviewed: bool,
    session_path: String,
    session_id: String,
    run_id: String,
    root: String,
    base_hash: Option<String>,
}
pub struct Adapter {
    pub store: Arc<dyn ReflectionStore>,
    pub roots: skillwrite::Roots,
    pub maximum: usize,
    pub require_review: bool,
}
impl Adapter {
    fn artifact(&self, request: &Request) -> Result<Artifact> {
        if super::digest(&request.subject.details) != request.subject.version {
            return Err(Error::Conflict);
        }
        let artifact: Artifact =
            serde_json::from_str(&request.subject.details).map_err(|_| Error::Unavailable)?;
        if artifact.root != self.roots.write_root.to_string_lossy()
            || (self.require_review && !artifact.reviewed)
        {
            return Err(Error::Conflict);
        }
        Ok(artifact)
    }
    fn vetted(&self, artifact: &Artifact) -> Result<guard::Vetted> {
        let slice = transcript::read_slice(Path::new(&artifact.session_path), None, &str::to_owned)
            .map_err(|_| Error::Unavailable)?;
        let entries: HashMap<_, _> = slice.entries.iter().map(|e| (e.id.as_str(), e)).collect();
        let checked = guard::check(artifact.candidate.clone(), &entries, &str::to_owned)
            .map_err(|_| Error::Conflict)?;
        Ok(guard::approve(
            checked,
            if artifact.reviewed {
                guard::Verdict::Allowed
            } else {
                guard::Verdict::NotRequested
            },
        ))
    }
}
impl Domain for Adapter {
    fn details(&self, request: &Request) -> Result<String> {
        let artifact = self.artifact(request)?;
        Ok(format!(
            "{:?} skill {}\nTarget: {}/{}\nBase version: {:?}\n\n---\nname: {}\ndescription: {}\n---\n\n{}",
            artifact.candidate.action,
            artifact.candidate.name,
            artifact.root,
            artifact.candidate.name,
            artifact.base_hash,
            artifact.candidate.name,
            artifact.candidate.description,
            guard::content(&artifact.candidate.body, &artifact.candidate.scripts)
        ))
    }
    fn apply(&self, request: &Request) -> Result<()> {
        if request.state != State::Approved {
            return Ok(());
        }
        let artifact = self.artifact(request)?;
        let vetted = self.vetted(&artifact)?;
        let expected = skillwrite::version(&vetted);
        if let Some(receipt) = self
            .store
            .generated(vetted.name())
            .map_err(|_| Error::Unavailable)?
            && receipt.run_id == artifact.run_id
            && receipt.hash == expected
            && skillwrite::still_owned(&self.roots, &receipt)
        {
            return Ok(());
        }
        let current = skillwrite::precheck(
            self.store.as_ref(),
            &self.roots,
            vetted.action(),
            vetted.name(),
            self.maximum,
        )
        .map_err(|_| Error::Conflict)?;
        if current != artifact.base_hash {
            return Err(Error::Conflict);
        }
        skillwrite::apply_on_base(
            self.store.as_ref(),
            &self.roots,
            &vetted,
            &artifact.run_id,
            &artifact.session_id,
            &chrono::Utc::now().to_rfc3339(),
            self.maximum,
            artifact.base_hash.as_deref(),
        )
        .map(|_| ())
        .map_err(|_| Error::Unavailable)
    }
}
#[allow(clippy::too_many_arguments)]
pub fn publish(
    service: &Arc<Service>,
    store: Arc<dyn ReflectionStore>,
    roots: &skillwrite::Roots,
    maximum: usize,
    workspace: &str,
    session_id: &str,
    session_path: &str,
    presentation_origin: Option<&crate::schedule::Origin>,
    run_id: &str,

    vetted: &guard::Vetted,
    reviewed: bool,
) -> Result<Request> {
    let base_hash = skillwrite::precheck(
        store.as_ref(),
        roots,
        vetted.action(),
        vetted.name(),
        maximum,
    )
    .map_err(|_| Error::Conflict)?;
    service.register_domain(
        workspace,
        "reflection_skill",
        Arc::new(Adapter {
            store,
            roots: roots.clone(),
            maximum,
            require_review: reviewed,
        }),
    )?;
    let artifact = Artifact {
        candidate: guard::Candidate {
            action: vetted.action(),
            name: vetted.name().into(),
            description: vetted.description().into(),
            body: vetted.body().into(),
            scripts: vetted.scripts().clone(),
            reason: vetted.reason().into(),
            cited: vetted.cited().to_vec(),
        },
        reviewed,
        session_path: session_path.into(),
        session_id: session_id.into(),
        run_id: run_id.into(),
        root: roots.write_root.to_string_lossy().into_owned(),
        base_hash,
    };
    let details = serde_json::to_string(&artifact).map_err(|_| Error::Unavailable)?;
    service.create_with_origin(
        Proposal {
            kind: Kind::Authorization,
            authority: Authority::Durable,
            scope: super::Scope {
                workspace: workspace.into(),
                session_id: session_id.into(),
                task_id: String::new(),
                run_id: run_id.into(),
            },
            subject: Subject {
                tool_call_id: String::new(),
                source: "reflection_skill".into(),
                id: format!("{run_id}:{}", vetted.name()),
                version: super::digest(&details),
                details,
            },
            title: format!("Review skill {}", vetted.name()),
            reason: vetted.reason().into(),
            persistent: false,
            blocking: false,
            expires_at: None,
        },
        presentation_origin.map_or("", crate::schedule::Origin::as_str),
    )
}
