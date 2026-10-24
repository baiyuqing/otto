//! Ordinary skill presentation tests.

use super::*;

fn contracted_skill(directory: &Path) -> skill::Skill {
    skill::Skill {
        name: "reviewer".to_string(),
        description: "reviews things".to_string(),
        contract: Some(skill::Contract {
            input: "a path".to_string(),
            output: "the result".to_string(),
        }),
        directory: directory.to_path_buf(),
        path: directory.join("SKILL.md"),
    }
}

#[test]
fn skill_list_line_keeps_the_name_and_contract_but_omits_the_description() {
    let dir = tempfile::tempdir().expect("tempdir");
    let skill = contracted_skill(dir.path());

    assert_eq!(skill_list_line(&skill), "\n- reviewer [contract]");
    assert!(!skill_list_line(&skill).contains(&skill.description));
}
