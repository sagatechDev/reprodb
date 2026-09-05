//! Application library for the `reprodb` CLI.
//!
//! Command parsing and application services live in this library so they can
//! be tested without spawning the compiled executable. `main.rs` remains the
//! composition root for process-level concerns.

pub mod application;
pub mod cli;
pub mod domain;
pub mod error;
pub mod infrastructure;

use cli::ProfileCommands;
use cli::output::OutputStyle;
pub use cli::{Cli, Commands};
use domain::{ProfileName, TenantLookup};
pub use error::{AppError, ErrorCategory};

pub fn execute(cli: Cli) -> Result<(), AppError> {
    tracing::debug!(command = cli.command.name(), "command received");
    let style = OutputStyle::stdout(cli.color);

    match cli.command {
        Commands::Setup(arguments) if arguments.preview => {
            print!("{}", cli::preview::setup(&style));
            Ok(())
        }
        Commands::Profile(arguments) => match arguments.command {
            ProfileCommands::Add(arguments) if arguments.preview => {
                let profile = ProfileName::try_from(arguments.name)?;
                print!("{}", cli::preview::profile_add(&style, &profile));
                Ok(())
            }
            _ => Err(AppError::CommandNotImplemented { command: "profile" }),
        },
        Commands::Doctor(arguments) if arguments.preview => {
            print!("{}", cli::preview::doctor(&style));
            Ok(())
        }
        Commands::Pull(arguments) if arguments.preview => {
            let tenant = TenantLookup::try_from(arguments.tenant)?;
            print!("{}", cli::preview::pull(&style, &tenant, arguments.fresh));
            Ok(())
        }
        command => Err(AppError::CommandNotImplemented {
            command: command.name(),
        }),
    }
}
