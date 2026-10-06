//! Terminal-independent text editing and prompt history.

#[derive(Default)]
pub(crate) struct History {
    lines: Vec<String>,
    index: Option<usize>,
    draft: String,
}

impl History {
    fn seeded(lines: Vec<String>) -> Self {
        Self {
            lines,
            ..Self::default()
        }
    }

    fn remember(&mut self, line: &str) {
        self.index = None;
        self.draft.clear();
        if !line.is_empty() {
            self.lines.push(line.to_string());
        }
    }

    fn previous(&mut self, current: &str) -> Option<String> {
        let index = match self.index {
            Some(0) => return None,
            Some(index) => index - 1,
            None => {
                let newest = self.lines.len().checked_sub(1)?;
                self.draft = current.to_string();
                newest
            }
        };
        self.index = Some(index);
        Some(self.lines[index].clone())
    }

    fn next(&mut self) -> Option<String> {
        let index = self.index? + 1;
        if let Some(line) = self.lines.get(index) {
            self.index = Some(index);
            return Some(line.clone());
        }
        self.index = None;
        Some(std::mem::take(&mut self.draft))
    }

    fn recalling(&self) -> bool {
        self.index.is_some()
    }

    fn stop_recall(&mut self) {
        self.index = None;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Edit {
    Insert(String),
    Newline,
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    LineStart,
    LineEnd,
    PreviousHistory,
    NextHistory,
    Clear,
}

pub(crate) struct Composer {
    pub input: Vec<char>,
    pub cursor: usize,
    pub suggestion: usize,
    history: History,
}

impl Composer {
    pub(crate) fn new(history: Vec<String>) -> Self {
        Self {
            input: Vec::new(),
            cursor: 0,
            suggestion: 0,
            history: History::seeded(history),
        }
    }

    pub(crate) fn edit(&mut self, edit: Edit) {
        match edit {
            Edit::Insert(text) => self.insert_text(&text),
            Edit::Newline => {
                self.input.insert(self.cursor, '\n');
                self.cursor += 1;
                self.edited();
            }
            Edit::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.input.remove(self.cursor);
                }
                self.edited();
            }
            Edit::Delete => {
                if self.cursor < self.input.len() {
                    self.input.remove(self.cursor);
                }
                self.edited();
            }
            Edit::Left => self.cursor = self.cursor.saturating_sub(1),
            Edit::Right => self.cursor = (self.cursor + 1).min(self.input.len()),
            Edit::Home => self.cursor = 0,
            Edit::End => self.cursor = self.input.len(),
            Edit::LineStart => self.cursor = self.line_start(),
            Edit::LineEnd => self.cursor = self.line_end(),
            Edit::PreviousHistory => {
                let current = self.input.iter().collect::<String>();
                if let Some(line) = self.history.previous(&current) {
                    self.replace_input(&line);
                }
            }
            Edit::NextHistory => {
                if let Some(line) = self.history.next() {
                    self.replace_input(&line);
                }
            }
            Edit::Clear => {
                self.input.clear();
                self.cursor = 0;
                self.edited();
            }
        }
    }

    pub(crate) fn set_input(&mut self, value: &str) {
        self.replace_input(value);
    }

    pub(crate) fn take(&mut self) -> Option<String> {
        let line = self.input.iter().collect::<String>().trim().to_string();
        self.input.clear();
        self.cursor = 0;
        self.suggestion = 0;
        if !line.is_empty() {
            self.history.remember(&line);
        }
        (!line.is_empty()).then_some(line)
    }

    pub(crate) fn remember(&mut self, line: &str) {
        self.history.remember(line);
    }

    pub(crate) fn recalling(&self) -> bool {
        self.history.recalling()
    }

    pub(crate) fn insert_text(&mut self, text: &str) {
        self.input.splice(self.cursor..self.cursor, text.chars());
        self.cursor += text.chars().count();
        self.edited();
    }

    fn replace_input(&mut self, value: &str) {
        self.input = value.chars().collect();
        self.cursor = self.input.len();
        self.suggestion = 0;
    }

    fn edited(&mut self) {
        self.suggestion = 0;
        self.history.stop_recall();
    }

    fn line_start(&self) -> usize {
        self.input[..self.cursor]
            .iter()
            .rposition(|&ch| ch == '\n')
            .map_or(0, |index| index + 1)
    }

    fn line_end(&self) -> usize {
        self.input[self.cursor..]
            .iter()
            .position(|&ch| ch == '\n')
            .map_or(self.input.len(), |index| self.cursor + index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_round_trip_restores_the_interrupted_draft() {
        let mut composer = Composer::new(vec!["first".into(), "/model".into()]);
        composer.set_input("draft");
        composer.edit(Edit::PreviousHistory);
        assert_eq!(composer.input.iter().collect::<String>(), "/model");
        composer.edit(Edit::PreviousHistory);
        assert_eq!(composer.input.iter().collect::<String>(), "first");
        composer.edit(Edit::PreviousHistory);
        assert_eq!(composer.input.iter().collect::<String>(), "first");
        composer.edit(Edit::NextHistory);
        assert_eq!(composer.input.iter().collect::<String>(), "/model");
        composer.edit(Edit::NextHistory);
        assert_eq!(composer.input.iter().collect::<String>(), "draft");
        assert!(!composer.recalling());
        composer.edit(Edit::NextHistory);
        assert_eq!(composer.input.iter().collect::<String>(), "draft");
    }

    #[test]
    fn empty_history_leaves_the_composer_alone() {
        let mut composer = Composer::new(Vec::new());
        composer.set_input("draft");
        composer.edit(Edit::PreviousHistory);
        assert_eq!(composer.input.iter().collect::<String>(), "draft");
        assert!(!composer.recalling());
    }

    #[test]
    fn edits_use_character_offsets_and_deletion_respects_unicode() {
        let mut composer = Composer::new(Vec::new());
        composer.insert_text("猫é");
        composer.edit(Edit::Left);
        composer.insert_text("語");
        assert_eq!(composer.input.iter().collect::<String>(), "猫語é");
        composer.edit(Edit::Backspace);
        assert_eq!(composer.input.iter().collect::<String>(), "猫é");
        assert_eq!(composer.cursor, 1);
    }

    #[test]
    fn take_trims_remembers_and_resets_the_composer() {
        let mut composer = Composer::new(Vec::new());
        composer.insert_text("  hello  ");
        assert_eq!(composer.take().as_deref(), Some("hello"));
        assert!(composer.input.is_empty());
        assert_eq!(composer.cursor, 0);
        composer.edit(Edit::PreviousHistory);
        assert_eq!(composer.input.iter().collect::<String>(), "hello");
    }
}
