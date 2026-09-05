//! Application library for the `reprodb` CLI.
//!
//! Command parsing and application services live in this library so they can
//! be tested without spawning the compiled executable. `main.rs` remains the
//! composition root for process-level concerns.

pub mod cli;

use thiserror::Error;

pub use cli::{Cli, Commands};

#[derive(Debug, Error)]
#[error("command `{command}` is not implemented yet")]
pub struct CommandNotImplemented {
    command: &'static str,
}

pub fn execute(cli: Cli) -> Result<(), CommandNotImplemented> {
    Err(CommandNotImplemented {
        command: cli.command.name(),
    })
}
