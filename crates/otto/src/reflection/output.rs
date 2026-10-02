//! The reflection model's answer: strict parsing, then validation into a plan
//! of memory proposals.
//!
//! The answer is one JSON object. Unknown fields fail the whole run, because
//! a model that answers outside the contract cannot be trusted item by item.
//! Once the object parses, each proposal is checked on its own and dropped,
//! with a counted reason, if it fails; nothing is partially applied.
//!
//! Ownership: the plan owns its data. Nothing here touches the store.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::Deserialize;

use super::evidence::{self, Item};
use super::guard::Candidate as SkillCandidate;
use super::transcript::Entry;

pub const KINDS: &[&str] = &["preference", "fact", "convention"];
const MAXIMUM_KEY_BYTES: usize = 256;
const MAXIMUM_TEXT_BYTES: usize = 8 * 1024;
const MAXIMUM_REASON_BYTES: usize = 2 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Create,
    Update,
    Forget,
}

/// What a skill proposal asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SkillAction {
    Create,
    Revise,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScopeChoice {
    User,
    Workspace,
}

impl ScopeChoice {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Workspace => "workspace",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Output {
    #[serde(default)]
    memories: Vec<RawMemory>,
    #[serde(default)]
    skills: Vec<RawSkill>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSkill {
    action: String,
    name: String,
    description: String,
    body: String,
    reason: String,
    #[serde(default)]
    evidence: Vec<Item>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMemory {
    action: String,
    scope: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    key: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    confidence: f64,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    target_id: String,
    #[serde(default)]
    evidence: Vec<Item>,
}

/// A memory record the model was shown, so it can update or forget it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Existing {
    pub id: String,
    pub scope: ScopeChoice,
    pub kind: String,
    pub key: String,
    pub text: String,
    pub revision: u64,
}

/// One validated proposal, ready to become a memory candidate.
#[derive(Debug, Clone, PartialEq)]
pub struct Proposal {
    pub action: Action,
    pub scope: ScopeChoice,
    pub kind: String,
    pub key: String,
    pub text: String,
    pub confidence: f64,
    pub reason: String,
    pub target_id: String,
    pub base_revision: u64,
    /// The distinct entry ids the proposal cites, verified.
    pub cited: Vec<String>,
}

#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    pub proposals: Vec<Proposal>,
    /// Skill proposals that passed the output contract and their evidence
    /// check; the structure checks, rule scan and review come later.
    pub skills: Vec<SkillCandidate>,
    /// Dropped proposals by reason.
    pub dropped: BTreeMap<&'static str, usize>,
}

impl Plan {
    pub fn dropped_total(&self) -> usize {
        self.dropped.values().sum()
    }

    fn drop(&mut self, reason: &'static str) {
        *self.dropped.entry(reason).or_default() += 1;
    }
}

/// Parses `raw` and validates every proposal against `entries` (the entries
/// the model was shown) and `existing` (the records it was shown).
///
/// `maximum_skills` is the most skill proposals to accept, or `None` when
/// skills were not requested, in which case any the model returns are dropped
/// and counted.
pub fn plan(
    raw: &str,
    entries: &[Entry],
    existing: &[Existing],
    maximum: usize,
    maximum_skills: Option<usize>,
) -> Result<Plan, String> {
    let output: Output = serde_json::from_str(strip_fence(raw))
        .map_err(|error| format!("reflection answer is not the expected JSON: {error}"))?;
    let by_id: HashMap<&str, &Entry> = entries.iter().map(|e| (e.id.as_str(), e)).collect();
    let known: HashMap<(&str, ScopeChoice), &Existing> = existing
        .iter()
        .map(|record| ((record.id.as_str(), record.scope), record))
        .collect();
    let mut taken: HashSet<(ScopeChoice, String, String)> = existing
        .iter()
        .map(|record| (record.scope, record.kind.clone(), record.key.clone()))
        .collect();

    let mut plan = Plan::default();
    for raw in output.memories {
        if plan.proposals.len() >= maximum {
            plan.drop("over_limit");
            continue;
        }
        match validate(raw, &by_id, &known, &mut taken) {
            Ok(proposal) => plan.proposals.push(proposal),
            Err(reason) => plan.drop(reason),
        }
    }
    for raw in output.skills {
        let Some(maximum_skills) = maximum_skills else {
            plan.drop("skill_not_requested");
            continue;
        };
        if plan.skills.len() >= maximum_skills {
            plan.drop("skill_over_limit");
            continue;
        }
        match validate_skill(raw, &by_id) {
            Ok(candidate) => plan.skills.push(candidate),
            Err(reason) => plan.drop(reason),
        }
    }
    Ok(plan)
}

/// A skill must cite a user message (the user asked for or approved the work)
/// and an entry showing the procedure was actually performed.
fn validate_skill(
    raw: RawSkill,
    entries: &HashMap<&str, &Entry>,
) -> Result<SkillCandidate, &'static str> {
    let action = match raw.action.as_str() {
        "create" => SkillAction::Create,
        "revise" => SkillAction::Revise,
        _ => return Err("skill_bad_action"),
    };
    let requirement = evidence::Requirement {
        user: true,
        performed: true,
    };
    let cited = evidence::verify(&raw.evidence, entries, requirement).map_err(|f| f.reason())?;
    Ok(SkillCandidate {
        action,
        name: raw.name.trim().to_owned(),
        description: raw.description.trim().to_owned(),
        body: raw.body.trim().to_owned(),
        reason: raw.reason.trim().to_owned(),
        cited,
    })
}

fn validate(
    raw: RawMemory,
    entries: &HashMap<&str, &Entry>,
    known: &HashMap<(&str, ScopeChoice), &Existing>,
    taken: &mut HashSet<(ScopeChoice, String, String)>,
) -> Result<Proposal, &'static str> {
    let action = match raw.action.as_str() {
        "create" => Action::Create,
        "update" => Action::Update,
        "forget" => Action::Forget,
        _ => return Err("bad_action"),
    };
    let scope = match raw.scope.as_str() {
        "user" => ScopeChoice::User,
        "workspace" => ScopeChoice::Workspace,
        _ => return Err("bad_scope"),
    };
    let reason = raw.reason.trim().to_owned();
    if reason.is_empty() || reason.len() > MAXIMUM_REASON_BYTES || has_control(&reason) {
        return Err("bad_reason");
    }
    let confidence = if raw.confidence.is_finite() {
        raw.confidence.clamp(0.0, 1.0)
    } else {
        return Err("bad_confidence");
    };

    let (kind, key, text, target_id, base_revision) = match action {
        Action::Create => {
            let kind = raw.kind.trim().to_owned();
            let key = raw.key.trim().to_owned();
            let text = raw.text.trim().to_owned();
            if !KINDS.contains(&kind.as_str()) {
                return Err("bad_kind");
            }
            check_content(&key, &text)?;
            if !raw.target_id.is_empty() {
                return Err("bad_target");
            }
            if !taken.insert((scope, kind.clone(), key.clone())) {
                return Err("already_exists");
            }
            (kind, key, text, String::new(), 0)
        }
        Action::Update => {
            let target = known
                .get(&(raw.target_id.as_str(), scope))
                .ok_or("unknown_target")?;
            let text = raw.text.trim().to_owned();
            check_content(&target.key, &text)?;
            if text == target.text {
                return Err("no_change");
            }
            (
                target.kind.clone(),
                target.key.clone(),
                text,
                target.id.clone(),
                target.revision,
            )
        }
        Action::Forget => {
            let target = known
                .get(&(raw.target_id.as_str(), scope))
                .ok_or("unknown_target")?;
            (
                String::new(),
                String::new(),
                String::new(),
                target.id.clone(),
                target.revision,
            )
        }
    };

    let require_user = action != Action::Create || kind == "preference";
    let requirement = evidence::Requirement {
        user: require_user,
        performed: false,
    };
    let cited = evidence::verify(&raw.evidence, entries, requirement).map_err(|f| f.reason())?;
    Ok(Proposal {
        action,
        scope,
        kind,
        key,
        text,
        confidence: if action == Action::Forget {
            0.0
        } else {
            confidence
        },
        reason,
        target_id,
        base_revision,
        cited,
    })
}

fn check_content(key: &str, text: &str) -> Result<(), &'static str> {
    if key.is_empty() || key.len() > MAXIMUM_KEY_BYTES || has_control(key) {
        return Err("bad_key");
    }
    if text.is_empty() || text.len() > MAXIMUM_TEXT_BYTES || has_control(text) {
        return Err("bad_text");
    }
    Ok(())
}

fn has_control(text: &str) -> bool {
    text.chars()
        .any(|character| character.is_control() && character != '\n' && character != '\t')
}

/// Removes one surrounding Markdown code fence, which models often add around
/// JSON. Anything else is left for the strict parser to reject.
fn strip_fence(raw: &str) -> &str {
    let trimmed = raw.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let rest = rest.strip_prefix("json").unwrap_or(rest);
    rest.strip_suffix("```").map_or(trimmed, str::trim)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reflection::transcript::EntryRole;

    /// Memory-only planning, the shape most tests exercise.
    fn plan(
        raw: &str,
        entries: &[Entry],
        existing: &[Existing],
        maximum: usize,
    ) -> Result<Plan, String> {
        super::plan(raw, entries, existing, maximum, None)
    }

    fn entries() -> Vec<Entry> {
        vec![
            Entry {
                id: "a0000001".into(),
                role: EntryRole::User,
                tool: String::new(),
                is_error: false,
                external: false,
                text: "Always answer in Chinese. I prefer tabs over spaces.".into(),
            },
            Entry {
                id: "a0000002".into(),
                role: EntryRole::Tool,
                tool: "read".into(),
                is_error: false,
                external: false,
                text: "the build uses cargo nextest".into(),
            },
        ]
    }

    fn existing() -> Vec<Existing> {
        vec![Existing {
            id: "m1".into(),
            scope: ScopeChoice::User,
            kind: "preference".into(),
            key: "language".into(),
            text: "Answer in English".into(),
            revision: 3,
        }]
    }

    fn memory(extra: &str) -> String {
        format!(
            r#"{{"memories":[{{"action":"create","scope":"user","kind":"preference","key":"language","reason":"user said so",
            "text":"Answer in Chinese","confidence":0.9,
            "evidence":[{{"entry":"a0000001","quote":"Always answer in Chinese"}}]{extra}}}]}}"#
        )
    }

    #[test]
    fn a_valid_create_is_planned_with_its_citations() {
        let plan = plan(
            &memory("").replace("language", "reply-language"),
            &entries(),
            &existing(),
            8,
        )
        .expect("plan");
        assert_eq!(plan.proposals.len(), 1);
        assert_eq!(plan.proposals[0].cited, vec!["a0000001".to_owned()]);
        assert_eq!(plan.dropped_total(), 0);
    }

    #[test]
    fn unknown_fields_and_bad_json_fail_the_run() {
        assert!(plan(r#"{"memories":[],"extra":1}"#, &entries(), &[], 8).is_err());
        assert!(plan("not json", &entries(), &[], 8).is_err());
        assert!(plan(&memory(r#","unknown":1"#), &entries(), &[], 8).is_err());
    }

    #[test]
    fn a_fenced_answer_is_accepted() {
        let fenced = format!("```json\n{}\n```", r#"{"memories":[]}"#);
        assert!(
            plan(&fenced, &entries(), &[], 8)
                .expect("plan")
                .proposals
                .is_empty()
        );
    }

    #[test]
    fn a_proposal_without_matching_evidence_is_dropped_and_counted() {
        let answer = memory("").replace("Always answer in Chinese\"}", "Never answer in French\"}");
        let plan = plan(&answer.replace("language", "reply"), &entries(), &[], 8).expect("plan");
        assert!(plan.proposals.is_empty());
        assert_eq!(plan.dropped.get("evidence_quote_mismatch"), Some(&1));
    }

    #[test]
    fn a_create_over_an_existing_key_is_dropped() {
        let plan = plan(&memory(""), &entries(), &existing(), 8).expect("plan");
        assert!(plan.proposals.is_empty());
        assert_eq!(plan.dropped.get("already_exists"), Some(&1));
    }

    #[test]
    fn an_update_keeps_the_targets_identity_and_revision() {
        let answer = r#"{"memories":[{"action":"update","scope":"user","target_id":"m1",
          "text":"Answer in Chinese","confidence":0.8,"reason":"user changed it",
          "evidence":[{"entry":"a0000001","quote":"Always answer in Chinese"}]}]}"#;
        let plan = plan(answer, &entries(), &existing(), 8).expect("plan");
        let proposal = &plan.proposals[0];
        assert_eq!(
            (
                proposal.kind.as_str(),
                proposal.key.as_str(),
                proposal.base_revision
            ),
            ("preference", "language", 3)
        );
    }

    #[test]
    fn update_and_forget_need_a_known_target_in_the_same_scope() {
        let answer = r#"{"memories":[
          {"action":"forget","scope":"workspace","target_id":"m1","reason":"r",
           "evidence":[{"entry":"a0000001","quote":"Always answer in Chinese"}]},
          {"action":"forget","scope":"user","target_id":"nope","reason":"r",
           "evidence":[{"entry":"a0000001","quote":"Always answer in Chinese"}]}]}"#;
        let plan = plan(answer, &entries(), &existing(), 8).expect("plan");
        assert!(plan.proposals.is_empty());
        assert_eq!(plan.dropped.get("unknown_target"), Some(&2));
    }

    #[test]
    fn a_forget_proposal_carries_no_content() {
        let answer = r#"{"memories":[{"action":"forget","scope":"user","target_id":"m1",
          "text":"ignored","confidence":0.9,"reason":"user asked",
          "evidence":[{"entry":"a0000001","quote":"Always answer in Chinese"}]}]}"#;
        let plan = plan(answer, &entries(), &existing(), 8).expect("plan");
        let proposal = &plan.proposals[0];
        assert!(proposal.kind.is_empty() && proposal.key.is_empty() && proposal.text.is_empty());
        assert_eq!(proposal.confidence, 0.0);
    }

    #[test]
    fn a_preference_must_cite_the_user_but_a_fact_may_cite_a_tool_result() {
        let fact = r#"{"memories":[{"action":"create","scope":"workspace","kind":"fact","key":"test-runner",
          "text":"Tests run with cargo nextest","confidence":0.7,"reason":"read from config",
          "evidence":[{"entry":"a0000002","quote":"cargo nextest"}]}]}"#;
        assert_eq!(
            plan(fact, &entries(), &[], 8)
                .expect("plan")
                .proposals
                .len(),
            1
        );
        let preference = fact.replace("\"fact\"", "\"preference\"");
        let plan = plan(&preference, &entries(), &[], 8).expect("plan");
        assert_eq!(plan.dropped.get("evidence_no_user_entry"), Some(&1));
    }

    #[test]
    fn proposals_past_the_limit_are_dropped_and_counted() {
        let item = |key: &str| {
            format!(
                r#"{{"action":"create","scope":"user","kind":"fact","key":"{key}","text":"t t t t","confidence":0.5,
                "reason":"r","evidence":[{{"entry":"a0000001","quote":"I prefer tabs over spaces"}}]}}"#
            )
        };
        let answer = format!(
            r#"{{"memories":[{},{},{}]}}"#,
            item("a"),
            item("b"),
            item("c")
        );
        let plan = plan(&answer, &entries(), &[], 2).expect("plan");
        assert_eq!(plan.proposals.len(), 2);
        assert_eq!(plan.dropped.get("over_limit"), Some(&1));
    }

    #[test]
    fn invalid_actions_scopes_kinds_and_confidence_are_dropped() {
        let answer = r#"{"memories":[
          {"action":"nuke","scope":"user","reason":"r","evidence":[]},
          {"action":"create","scope":"galaxy","reason":"r","evidence":[]},
          {"action":"create","scope":"user","kind":"secret","key":"k","text":"t t t t","reason":"r","evidence":[]}]}"#;
        let plan = plan(answer, &entries(), &[], 8).expect("plan");
        assert_eq!(plan.dropped.get("bad_action"), Some(&1));
        assert_eq!(plan.dropped.get("bad_scope"), Some(&1));
        assert_eq!(plan.dropped.get("bad_kind"), Some(&1));
    }

    fn skill(extra_evidence: &str) -> String {
        format!(
            r#"{{"skills":[{{"action":"create","name":"cargo-lint","description":"Lint before committing",
            "body":"1. Run cargo fmt.\n2. Run cargo clippy.\n3. Fix every warning.","reason":"it worked",
            "evidence":[{{"entry":"a0000001","quote":"Always answer in Chinese"}}{extra_evidence}]}}]}}"#
        )
    }

    fn plan_skills(raw: &str, maximum: Option<usize>) -> Plan {
        super::plan(raw, &entries(), &[], 8, maximum).expect("plan")
    }

    #[test]
    fn a_skill_needs_a_user_citation_and_proof_the_work_was_performed() {
        let performed = r#",{"entry":"a0000002","quote":"cargo nextest"}"#;
        let plan = plan_skills(&skill(performed), Some(2));
        assert_eq!(plan.skills.len(), 1, "{:?}", plan.dropped);
        assert_eq!(plan.skills[0].name, "cargo-lint");
        assert_eq!(plan.skills[0].action, SkillAction::Create);

        let plan = plan_skills(&skill(""), Some(2));
        assert!(plan.skills.is_empty());
        assert_eq!(plan.dropped.get("evidence_not_performed"), Some(&1));
    }

    #[test]
    fn skills_are_dropped_when_not_requested_or_over_the_limit() {
        let performed = r#",{"entry":"a0000002","quote":"cargo nextest"}"#;
        let one = skill(performed);
        let plan = plan_skills(&one, None);
        assert!(plan.skills.is_empty());
        assert_eq!(plan.dropped.get("skill_not_requested"), Some(&1));

        let item = one
            .strip_prefix(r#"{"skills":["#)
            .and_then(|text| text.strip_suffix("]}"))
            .expect("one skill")
            .to_owned();
        let two = format!(r#"{{"skills":[{item},{item}]}}"#);
        let plan = plan_skills(&two, Some(1));
        assert_eq!(plan.skills.len(), 1);
        assert_eq!(plan.dropped.get("skill_over_limit"), Some(&1));
    }

    #[test]
    fn a_skill_with_an_unknown_action_or_field_is_rejected() {
        let performed = r#",{"entry":"a0000002","quote":"cargo nextest"}"#;
        let bad = skill(performed).replace("\"create\"", "\"delete\"");
        let plan = plan_skills(&bad, Some(2));
        assert_eq!(plan.dropped.get("skill_bad_action"), Some(&1));
        let extra = skill(performed).replace("\"reason\"", "\"allowed-tools\":\"bash\",\"reason\"");
        assert!(super::plan(&extra, &entries(), &[], 8, Some(2)).is_err());
    }
}
