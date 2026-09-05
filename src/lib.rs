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
use infrastructure::config::ConfigRepository;

pub async fn execute(cli: Cli) -> Result<(), AppError> {
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
            ProfileCommands::Add(arguments) => {
                let name = ProfileName::try_from(arguments.name)?;
                let service = application::ProfileService::new(ConfigRepository::discover()?);
                service.ensure_name_available(&name)?;
                println!("{}", cli::profile::render_add_intro(&style, &name));
                let input = cli::prompt::collect_new_profile(name)?;
                println!(
                    "\n{}",
                    cli::profile::render_verifying(&style, input.tls_mode)
                );
                let verifier = infrastructure::mysql::DockerSourceProfileVerifier::new(
                    infrastructure::process::TokioProcessRunner,
                );
                let created = service
                    .add(
                        &infrastructure::credentials::OsCredentialStore,
                        &verifier,
                        input,
                    )
                    .await?;
                print!("{}", cli::profile::render_created(&style, &created));
                Ok(())
            }
            ProfileCommands::List => {
                let service = application::ProfileService::new(ConfigRepository::discover()?);
                let profiles = service.list()?;
                print!("{}", cli::profile::render_list(&style, &profiles));
                Ok(())
            }
            ProfileCommands::Use(arguments) => {
                let name = ProfileName::try_from(arguments.name)?;
                let service = application::ProfileService::new(ConfigRepository::discover()?);
                service.activate(&name)?;
                print!("{}", cli::profile::render_activated(&style, &name));
                Ok(())
            }
            ProfileCommands::Remove(arguments) => {
                let name = ProfileName::try_from(arguments.name)?;
                let confirmed = arguments.yes || cli::prompt::confirm_profile_removal(&name)?;
                if !confirmed {
                    print!("{}", cli::profile::render_removal_cancelled(&style));
                    return Ok(());
                }
                let service = application::ProfileService::new(ConfigRepository::discover()?);
                let removal = service
                    .remove(&infrastructure::credentials::OsCredentialStore, &name)
                    .await?;
                print!("{}", cli::profile::render_removed(&style, &name, removal));
                Ok(())
            }
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
