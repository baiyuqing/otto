//! Read-only skill browsing with explicit, persisted enablement changes.
use super::{Presenter, Transition};
use crate::app::management::{ManagedSkill, Reply as ManagementReply, Request as Management};
use crate::tui::backend::{Command, Reply, Request};
use crate::tui::interaction::{Action, Activation, Row, View};

#[derive(Default)]
pub(crate) struct Skills {
    rows: Vec<ManagedSkill>,
    loaded: bool,
    global_enabled: bool,
    query: String,
    saving: Option<(String, bool)>,
    notice: Option<String>,
}

impl Presenter for Skills {
    fn key(&self) -> String {
        "skills".into()
    }
    fn view(&self) -> View {
        let query = self.query.to_lowercase();
        let rows: Vec<_> = self
            .rows
            .iter()
            .filter(|row| {
                row.skill.name.to_lowercase().contains(&query)
                    || row.skill.description.to_lowercase().contains(&query)
            })
            .map(|row| Row {
                id: row.skill.name.clone(),
                cells: vec![format!(
                    "[{}] {} — {}",
                    if !self.global_enabled {
                        "globally disabled"
                    } else if row.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    row.skill.name,
                    row.skill.description
                )],
                detail: format!(
                    "{}\n\nPer-skill preference: {} · Session catalog: {}\nLocation: {}",
                    row.skill.description,
                    if row.enabled { "enabled" } else { "disabled" },
                    if row.loaded { "loaded" } else { "not loaded" },
                    row.skill.directory.display()
                ),
            })
            .collect();
        View {
            title: "Skills".into(),
            description: format!(
                "Search: {} · type to filter · {} · Enter details",
                self.query,
                if self.global_enabled {
                    "Space toggle"
                } else {
                    "Skills globally disabled: set [skills].enabled = true in Otto config and restart Otto; toggles unavailable"
                }
            ),
            collection: true,
            search: Some(self.query.clone()),
            loading: !self.loaded,
            document: if self.loaded && rows.is_empty() {
                if !self.global_enabled { "Skills globally disabled. Set [skills].enabled = true in Otto config and restart Otto; restarting alone will not enable skills." } else if self.rows.is_empty() { "No skills discovered. Add a SKILL.md under a configured skill root and restart Otto." } else { "No matching skills. Backspace to edit the search." }.into()
            } else {
                String::new()
            },
            actions: vec![Action {
                id: "toggle".into(),
                label: "Toggle configured state".into(),
                enabled: self.global_enabled && !rows.is_empty() && self.saving.is_none(),
                shortcut: Some(' '),
                ..Default::default()
            }],
            rows,
            notice: self
                .saving
                .as_ref()
                .map(|(name, _)| format!("Saving {name}… configured state is not confirmed yet"))
                .or_else(|| self.notice.clone()),
            ..Default::default()
        }
    }
    fn activate(&mut self, activation: Activation) -> Transition {
        match activation {
            Activation::Open(name) if self.saving.is_none() => {
                Transition::Push(Box::new(Detail { name, text: None }))
            }
            Activation::Action { id, fields, .. } if id == "otto:search" => {
                self.query = fields.get("query").cloned().unwrap_or_default();
                Transition::Stay
            }
            Activation::Action {
                id,
                row: Some(name),
                ..
            } if id == "toggle" && self.global_enabled && self.saving.is_none() => {
                if let Some(row) = self.rows.iter().find(|row| row.skill.name == name) {
                    let enabled = !row.enabled;
                    self.saving = Some((name.clone(), enabled));
                    self.notice = None;
                    Transition::Request(Request::Command(Command::Management(
                        Management::SkillSet { name, enabled },
                    )))
                } else {
                    Transition::Stay
                }
            }
            _ => Transition::Stay,
        }
    }
    fn receive(&mut self, reply: Reply) -> Transition {
        match reply {
            Reply::Management(ManagementReply::SkillManager(inventory)) => {
                self.global_enabled = inventory.global_enabled;
                self.rows = inventory.skills;
                self.loaded = true;
            }
            Reply::Management(ManagementReply::SkillSet(message)) => {
                if let Some((name, enabled)) = self.saving.take() {
                    if let Some(row) = self.rows.iter_mut().find(|row| row.skill.name == name) {
                        row.enabled = enabled;
                    }
                }
                self.notice = Some(message);
                return Transition::Request(Request::Management(Management::SkillManager));
            }
            _ => {}
        }
        Transition::Stay
    }
    fn failed(&mut self, error: &str) {
        self.saving = None;
        self.notice = Some(format!(
            "Not confirmed: {error}. Configured state unchanged here; retry or reopen /skill to read persisted state."
        ));
    }
    fn refresh(&self) -> Option<Request> {
        Some(Request::Management(Management::SkillManager))
    }
}

struct Detail {
    name: String,
    text: Option<String>,
}
impl Presenter for Detail {
    fn key(&self) -> String {
        format!("skill:{}", self.name)
    }
    fn view(&self) -> View {
        View {
            title: format!("Skill: {} (read-only)", self.name),
            description: "↑/↓ / PgUp/PgDn scroll · Esc back · Viewing never executes this skill"
                .into(),
            loading: self.text.is_none(),
            document: self.text.clone().unwrap_or_default(),
            ..Default::default()
        }
    }
    fn activate(&mut self, _: Activation) -> Transition {
        Transition::Stay
    }
    fn receive(&mut self, reply: Reply) -> Transition {
        if let Reply::Management(ManagementReply::SkillInfo(info)) = reply {
            let contract = info
                .skill
                .contract
                .as_ref()
                .map(|c| {
                    format!(
                        "\nContract input: {}\nContract output: {}",
                        c.input, c.output
                    )
                })
                .unwrap_or_default();
            self.text = Some(format!(
                "Description: {}\nLocation: {}\nSource file: {}{}\n{}\n\nInstructions (untrusted skill text):\n{}",
                info.skill.description,
                info.skill.directory.display(),
                info.skill.path.display(),
                contract,
                info.contract_check.as_deref().unwrap_or(""),
                info.body
            ));
        }
        Transition::Stay
    }
    fn refresh(&self) -> Option<Request> {
        Some(Request::Management(Management::SkillInfo(
            self.name.clone(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill::{Contract, Skill};
    use crate::tui::interaction::{Intent, State};
    use std::collections::BTreeMap;
    fn rows() -> Vec<ManagedSkill> {
        (0..80)
            .map(|i| ManagedSkill {
                skill: Skill {
                    name: format!("skill-{i:02}"),
                    description: format!("Description {i} — searchable"),
                    contract: Some(Contract {
                        input: "input".into(),
                        output: "output".into(),
                    }),
                    directory: "/skills/demo".into(),
                    path: "/skills/demo/SKILL.md".into(),
                },
                enabled: i != 0,
                loaded: i != 0,
            })
            .collect()
    }
    fn manager() -> Skills {
        let mut skills = Skills::default();
        skills.receive(Reply::Management(ManagementReply::SkillManager(
            crate::app::management::SkillManager {
                global_enabled: true,
                skills: rows(),
            },
        )));
        skills
    }
    fn toggle() -> Activation {
        Activation::Action {
            id: "toggle".into(),
            row: Some("skill-00".into()),
            fields: BTreeMap::new(),
        }
    }
    #[test]
    fn skill_manager_global_disabled_preserves_preferences_and_blocks_toggles() {
        let mut skills = manager();
        let inventory = crate::app::management::SkillManager {
            global_enabled: false,
            skills: rows(),
        };
        skills.receive(Reply::Management(ManagementReply::SkillManager(inventory)));
        let view = skills.view();
        assert!(view.description.contains("[skills].enabled = true"));
        assert!(view.description.contains("restart Otto"));
        assert!(view.rows[1].cells[0].contains("globally disabled"));
        assert!(view.rows[1].detail.contains("preference: enabled"));
        assert!(view.rows[1].detail.contains("loaded"));
        assert!(!view.actions[0].enabled);
        assert!(matches!(skills.activate(toggle()), Transition::Stay));
        assert!(skills.saving.is_none());
        assert!(!skills.rows[0].enabled);
        skills.rows.clear();
        assert!(
            skills
                .view()
                .document
                .contains("restarting alone will not enable")
        );
        assert!(!skills.view().document.contains("Add a SKILL.md"));
        // A refreshed persisted configuration can re-enable preference editing.
        skills.receive(Reply::Management(ManagementReply::SkillManager(
            crate::app::management::SkillManager {
                global_enabled: true,
                skills: rows(),
            },
        )));
        assert!(skills.view().actions[0].enabled);
    }

    #[test]
    fn skill_manager_search_select_scroll_and_empty_states() {
        let mut skills = manager();
        let view = skills.view();
        let mut state = State::new(&view);
        assert_eq!(state.selected_row.as_deref(), Some("skill-00"));
        assert!(view.rows[0].cells[0].contains("[disabled]"));
        assert!(view.rows[0].detail.contains("Description 0"));
        assert!(view.rows[0].detail.contains("not loaded"));
        state.page_rows = 5;
        state.handle(&view, Intent::Page(1));
        assert_eq!(state.selected_row.as_deref(), Some("skill-05"));
        state.handle(&view, Intent::Move(74));
        assert_eq!(state.selected_row.as_deref(), Some("skill-79"));
        // Terminal key translation emits Shortcut for printable keys in rows.
        for key in "description 23".chars().filter(|c| *c != ' ') {
            let activation = state.handle(&skills.view(), Intent::Shortcut(key)).unwrap();
            skills.activate(activation);
        }
        // Search matches name and description, including Unicode, case-insensitively.
        skills.query = "DESCRIPTION 23".into();
        state.reconcile(&skills.view());
        assert_eq!(skills.view().rows.len(), 1);
        assert_eq!(state.selected_row.as_deref(), Some("skill-23"));
        skills.query = "missing".into();
        assert!(skills.view().document.contains("No matching"));
        skills.rows.clear();
        assert!(skills.view().document.contains("No skills discovered"));
        assert_eq!(state.handle(&view, Intent::Back), Some(Activation::Back));
    }
    #[test]
    fn skill_manager_toggle_is_confirmed_only_on_success_and_retryable_on_failure() {
        let mut skills = manager();
        assert!(matches!(
            skills.activate(toggle()),
            Transition::Request(Request::Command(Command::Management(
                Management::SkillSet { enabled: true, .. }
            )))
        ));
        assert!(!skills.rows[0].enabled);
        assert!(skills.view().notice.unwrap().contains("Saving"));
        assert!(!skills.view().actions[0].enabled);
        assert!(matches!(skills.activate(toggle()), Transition::Stay));
        skills.failed("write denied");
        assert!(!skills.rows[0].enabled);
        assert!(skills.view().notice.unwrap().contains("write denied"));
        assert!(skills.view().actions[0].enabled);
        skills.activate(toggle());
        assert!(matches!(
            skills.receive(Reply::Management(ManagementReply::SkillSet(
                "enabled; restart Otto to apply the active catalog".into()
            ))),
            Transition::Request(_)
        ));
        assert!(skills.rows[0].enabled);
        assert!(!skills.rows[0].loaded);
        assert!(skills.view().notice.unwrap().contains("restart Otto"));
    }
    #[test]
    fn skill_manager_enter_details_never_executes_and_space_keeps_row_focus() {
        let mut skills = manager();
        let view = skills.view();
        let mut state = State::new(&view);
        let activation = state.handle(&view, Intent::Shortcut(' ')).unwrap();
        skills.activate(activation);
        assert!(matches!(
            state.handle(&view, Intent::Activate),
            Some(Activation::Open(_))
        ));
        skills.failed("test reset");
        let Transition::Push(mut detail) = skills.activate(Activation::Open("skill-00".into()))
        else {
            panic!("detail surface expected")
        };
        assert!(matches!(
            detail.start(),
            Some(Request::Management(Management::SkillInfo(_)))
        ));
        detail.receive(Reply::Management(ManagementReply::SkillInfo(
            crate::app::management::SkillInfo {
                skill: rows().remove(0).skill,
                body: "instruction\n".repeat(100),
                contract_check: Some("Contract check: valid".into()),
            },
        )));
        let view = detail.view();
        for text in [
            "Description 0",
            "Source file:",
            "SKILL.md",
            "Contract input: input",
            "Contract output: output",
            "Contract check: valid",
            "instruction",
        ] {
            assert!(view.document.contains(text), "{text}");
        }
        let mut state = State::new(&view);
        state.page_rows = 3;
        state.handle(&view, Intent::Page(1));
        assert_eq!(state.scroll, 3);
    }
}
