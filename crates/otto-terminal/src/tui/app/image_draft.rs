//! Local composer attachments, independent of text history and remote input.

use super::*;
use crossterm::event::KeyEventKind;
use otto_core::model::Block;

const MAX_IMAGES: usize = 4;

impl App {
    pub(super) fn image_draft_key(&mut self, key: KeyEvent) -> bool {
        if self.attached
            || !cfg!(target_os = "macos")
            || !key.modifiers.is_empty()
            || key.kind != KeyEventKind::Press
        {
            return false;
        }
        match key.code {
            KeyCode::F(8) => {
                self.add_image(super::super::image_input::clipboard);
            }
            KeyCode::F(9) => {
                self.draft_images.pop();
            }
            _ => return false,
        }
        true
    }

    pub(super) fn attach_image_command(&mut self, controller: &Controller, text: &str) -> bool {
        if self.attached || !cfg!(target_os = "macos") {
            return false;
        }
        let (command, argument) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        if command != "/attach-image" {
            return false;
        }
        let argument = argument.trim();
        if argument.is_empty() {
            self.push_system("Usage: /attach-image <PNG or JPEG path>");
        } else if self.add_image(|| {
            super::super::image_input::file(std::path::Path::new(argument), |path| {
                controller.can_read_image_attachment(path)
            })
        }) {
            self.composer.edit(Edit::Clear);
        }
        true
    }

    fn add_image(&mut self, load: impl FnOnce() -> Result<Block, String>) -> bool {
        if self.draft_images.len() >= MAX_IMAGES {
            self.push_system("Up to four images per prompt. Press F9 to remove the last one.");
            return false;
        }
        match load() {
            Ok(image) => {
                self.draft_images.push(image);
                true
            }
            Err(error) => {
                self.push_system(error);
                false
            }
        }
    }

    pub(super) fn submit_image_draft(&mut self) -> Option<Action> {
        if !self.connected
            || self.busy()
            || self.has_writes()
            || self.queued_input.is_some()
            || self.queued_steer.is_some()
        {
            self.push_system(
                "Images can only be sent in a new turn. Draft retained; retry when Otto is idle.",
            );
        } else {
            return Some(Action::ImagePrompt);
        }
        None
    }
}
