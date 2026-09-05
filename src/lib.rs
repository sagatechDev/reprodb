//! Application library for the `reprodb` CLI.
//!
//! Command parsing and application services live in this library so they can
//! be tested without spawning the compiled executable. `main.rs` remains the
//! composition root for process-level concerns.

pub mod cli;
pub mod domain;
pub mod error;

pub use cli::{Cli, Commands};
pub use error::{AppError, ErrorCategory};

pub fn execute(cli: Cli) -> Result<(), AppError> {
    tracing::debug!(command = cli.command.name(), "command received");

    Err(AppError::CommandNotImplemented {
        command: cli.command.name(),
    })
}
