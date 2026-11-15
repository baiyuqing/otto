//! The terminal frontend: the interactive TUI (`tui`) and the line REPL,
//! login and slash-command code it uses (`cli`). It builds on `otto` for the
//! runtime and on `otto-client` for the `--attach` mode.

pub mod cli;
pub mod tui;
