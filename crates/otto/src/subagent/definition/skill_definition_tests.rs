use super::*;
use crate::skill::{Contract, Skill};

fn contracted(name: &str) -> Skill {
    Skill {
        name: name.into(),
        description: "Normalizes scanner output.".into(),
        contract: Some(Contract {
            input: "a scanner JSON report path".into(),
            output: "a JSON list of records".into(),
        }),
        directory: format!("/skills/{name}").into(),
        path: format!("/skills/{name}/SKILL.md").into(),
    }
}

#[test]
fn a_contracted_skill_becomes_a_fresh_context_definition() {
    let definition =
        from_skill(&contracted("normalize"), "step one\nstep two".into()).expect("a definition");

    assert_eq!(definition.name, "normalize");
    assert_eq!(
        definition.context, "fresh",
        "inherit would reintroduce the context this exists to avoid"
    );
    assert!(
        definition.tools.is_none(),
        "a skill must not widen the child tool set"
    );
    assert_eq!(definition.write_policy, WritePolicy::default());
    assert!(definition.model.is_empty());
    assert!(definition.body.contains("step one"), "{}", definition.body);
}

#[test]
fn the_description_carries_both_halves_of_the_contract() {
    let definition = from_skill(&contracted("normalize"), "body".into()).expect("a definition");

    assert!(
        definition
            .description
            .contains("a scanner JSON report path"),
        "{}",
        definition.description
    );
    assert!(
        definition.description.contains("a JSON list of records"),
        "{}",
        definition.description
    );
    assert!(
        definition
            .description
            .contains("Normalizes scanner output."),
        "{}",
        definition.description
    );
}

#[test]
fn the_body_tells_the_child_how_to_reach_the_packages_own_files() {
    let definition = from_skill(&contracted("normalize"), "body".into()).expect("a definition");

    assert!(
        definition.body.contains("skill"),
        "the child must be told which tool reads the package: {}",
        definition.body
    );
    assert!(
        definition.body.contains("normalize"),
        "the note must name the skill: {}",
        definition.body
    );
}

#[test]
fn a_skill_without_a_contract_gets_no_definition() {
    let mut plain = contracted("plain");
    plain.contract = None;

    assert!(from_skill(&plain, "body".into()).is_none());
}
