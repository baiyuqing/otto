//! Domain projections and semantic feature transitions. No terminal events.

mod approval;
mod choice;
mod collection;
#[cfg(test)]
mod command_order_tests;
mod commands;
mod review;
pub(super) mod skills;
#[cfg(test)]
mod tests;

pub(crate) use approval::Approval;
pub(crate) use commands::Catalog;

use super::backend::{Reply, Request};
use super::interaction::{Activation, View};

pub(crate) trait Presenter: Send {
    fn key(&self) -> String;
    /// Identifies nested surfaces that form one interaction and close together.
    fn flow_key(&self) -> String {
        self.key()
    }
    fn view(&self) -> View;
    fn activate(&mut self, activation: Activation) -> Transition;
    fn receive(&mut self, reply: Reply) -> Transition;
    fn start(&mut self) -> Option<Request> {
        self.refresh()
    }
    fn failed(&mut self, _: &str) {}
    fn refresh(&self) -> Option<Request> {
        None
    }
    fn retry(&self) -> Option<Request> {
        self.refresh()
    }
    fn periodic(&self) -> bool {
        false
    }
    fn covers_approval(&self, _: &otto_core::approval::Request) -> bool {
        false
    }
    /// Whether approvals wait while this surface is on top; it shows them
    /// itself and opens one on request.
    fn defers_approvals(&self) -> bool {
        false
    }
    /// The approval snapshot, delivered with every approval poll.
    fn pending_approvals(&mut self, _: &[otto_core::approval::Request]) {}
}

pub(crate) enum Transition {
    Stay,
    Close,
    /// Close every surface in the current presenter's nested interaction.
    CloseFlow,
    Push(Box<dyn Presenter>),
    Request(Request),
    Run(Run),
}

pub(crate) enum Run {
    Prompt(String),
    Compact(String),
    Login(String),
    McpLogin(String),
    Exit,
}

pub(crate) struct Document {
    pub key: String,
    pub title: String,
    pub text: String,
}

impl Presenter for Document {
    fn key(&self) -> String {
        self.key.clone()
    }

    fn view(&self) -> View {
        View {
            title: self.title.clone(),
            document: self.text.clone(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _: Activation) -> Transition {
        Transition::Stay
    }

    fn receive(&mut self, _: Reply) -> Transition {
        Transition::Stay
    }
}
