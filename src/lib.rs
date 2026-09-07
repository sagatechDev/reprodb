//! Application library for the `reprodb` CLI.
//!
//! Command parsing and application services live in this library so they can
//! be tested without spawning the compiled executable. `main.rs` remains the
//! composition root for process-level concerns.

use std::io::Write as _;

pub mod application;
pub mod cli;
pub mod domain;
pub mod error;
pub mod infrastructure;

use cli::output::OutputStyle;
use cli::{CacheCommands, ProfileCommands};
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
        Commands::Setup(_) => {
            let repository = ConfigRepository::discover()?;
            let service = application::SetupService::new(repository);
            let discovery = infrastructure::docker::DockerTargetDiscovery::new(
                infrastructure::process::TokioProcessRunner,
            );
            let (docker_context, candidates) = discovery.discover().await?;
            print!(
                "{}",
                cli::setup::render_discovered(&style, &docker_context, &candidates)
            );
            if candidates.is_empty() {
                return Err(application::SetupServiceError::NoCandidates.into());
            }

            let selected = cli::prompt::select_local_target(&candidates)?;
            let candidate = &candidates[selected];
            if candidate.state != infrastructure::docker::ContainerState::Running {
                if !cli::prompt::confirm_container_start(candidate)? {
                    print!("{}", cli::setup::render_cancelled(&style));
                    return Ok(());
                }
                print!("{}", cli::setup::render_starting(&style, candidate));
                discovery
                    .start_container(&docker_context, &candidate.id)
                    .await?;
            }
            if service.has_local_target()? && !cli::prompt::confirm_target_replacement()? {
                print!("{}", cli::setup::render_cancelled(&style));
                return Ok(());
            }

            let input = cli::prompt::collect_local_target(docker_context, candidate)?;
            println!("\n{}", cli::setup::render_verifying(&style, candidate));
            let verifier = infrastructure::mysql::DockerLocalTargetVerifier::new(
                infrastructure::process::TokioProcessRunner,
            );
            let configured = service
                .configure(
                    &infrastructure::credentials::OsCredentialStore,
                    &verifier,
                    input,
                )
                .await?;
            print!("{}", cli::setup::render_configured(&style, &configured));
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
        Commands::Doctor(_) => {
            print!("{}", cli::doctor::render_start(&style));
            std::io::stdout().flush().map_err(AppError::Output)?;
            let service = application::DoctorService::new(ConfigRepository::discover()?);
            let docker = infrastructure::docker::DockerCliDoctorInspector::new(
                infrastructure::process::TokioProcessRunner,
            );
            let source = infrastructure::mysql::DockerSourceProfileVerifier::read_only(
                infrastructure::process::TokioProcessRunner,
            );
            let target = infrastructure::mysql::DockerLocalTargetVerifier::read_only(
                infrastructure::process::TokioProcessRunner,
            );
            let storage = infrastructure::filesystem::LocalFilesystemInspector;
            let report = service
                .run(
                    &infrastructure::credentials::OsCredentialStore,
                    &docker,
                    &storage,
                    &source,
                    &target,
                )
                .await;
            print!("{}", cli::doctor::render(&style, &report));
            if let Some(kind) = report.first_failure_kind() {
                return Err(AppError::DoctorChecksFailed { kind });
            }
            Ok(())
        }
        Commands::Dump(arguments) => {
            let tenant = TenantLookup::try_from(arguments.tenant)?;
            print!("{}", cli::dump::render_start(&style));
            std::io::stdout().flush().map_err(AppError::Output)?;
            let repository = ConfigRepository::discover()?;
            let workflow = infrastructure::mysql::DockerDumpWorkflow::new(
                infrastructure::process::TokioProcessRunner,
            );
            let progress = std::sync::Arc::new(cli::dump::CliDumpProgress::new(style));
            let service = application::DumpService::new(repository).with_progress(progress.clone());
            let result = service
                .create(
                    &infrastructure::credentials::OsCredentialStore,
                    &workflow,
                    &workflow,
                    &infrastructure::mysql::DockerMysqlDumpExecutor,
                    tenant,
                )
                .await;
            progress.finish();
            let created = result?;
            print!("{}", cli::dump::render_complete(&style, &created));
            Ok(())
        }
        Commands::Restore(arguments) => {
            let tenant = TenantLookup::try_from(arguments.tenant)?;
            let dump_id = arguments.dump_id.parse::<domain::DumpId>()?;
            let target_database = arguments
                .database
                .map(domain::DatabaseName::try_from)
                .transpose()?;
            print!("{}", cli::restore::render_start(&style));
            std::io::stdout().flush().map_err(AppError::Output)?;

            let repository = ConfigRepository::discover()?;
            let progress = std::sync::Arc::new(cli::restore::CliRestoreProgress::new(style));
            let service = application::RestoreService::new(repository).with_progress(progress);
            let restored = service
                .restore_to(
                    &infrastructure::credentials::OsCredentialStore,
                    &infrastructure::mysql::DockerLocalTargetAttestor::new(
                        infrastructure::process::TokioProcessRunner,
                    ),
                    infrastructure::mysql::DockerMysqlRestoreExecutor,
                    infrastructure::mysql::DockerLocalTenantWriter::new(
                        infrastructure::process::TokioProcessRunner,
                    ),
                    application::RestoreRequest {
                        tenant,
                        dump_id,
                        target_database,
                    },
                )
                .await?;
            print!("{}", cli::restore::render_complete(&style, &restored));
            Ok(())
        }
        Commands::Pull(arguments) if arguments.preview => {
            let tenant = TenantLookup::try_from(arguments.tenant)?;
            let target_database = arguments
                .database
                .map(domain::DatabaseName::try_from)
                .transpose()?;
            print!(
                "{}",
                cli::preview::pull(&style, &tenant, arguments.fresh, target_database.as_ref())
            );
            Ok(())
        }
        Commands::Pull(arguments) => {
            let tenant = TenantLookup::try_from(arguments.tenant)?;
            let target_database = arguments
                .database
                .map(domain::DatabaseName::try_from)
                .transpose()?;
            let database_selector = cli::pull::CliPullDatabaseSelector::new(target_database);
            print!("{}", cli::pull::render_start(&style));
            std::io::stdout().flush().map_err(AppError::Output)?;

            let repository = ConfigRepository::discover()?;
            let workflow = infrastructure::mysql::DockerDumpWorkflow::new(
                infrastructure::process::TokioProcessRunner,
            );
            let progress = std::sync::Arc::new(cli::pull::CliPullProgress::new(style));
            let service = application::PullService::new(repository)
                .with_progress(progress.clone())
                .with_compression_progress(progress.clone())
                .with_restore_progress(progress.clone());
            let result = service
                .pull(
                    &infrastructure::credentials::OsCredentialStore,
                    application::PullDumpDependencies {
                        tenant_resolver: &workflow,
                        preflight: &workflow,
                        executor: &infrastructure::mysql::DockerMysqlDumpExecutor,
                    },
                    application::PullRestoreDependencies {
                        target_attestor: &infrastructure::mysql::DockerLocalTargetAttestor::new(
                            infrastructure::process::TokioProcessRunner,
                        ),
                        executor: infrastructure::mysql::DockerMysqlRestoreExecutor,
                        tenant_writer: infrastructure::mysql::DockerLocalTenantWriter::new(
                            infrastructure::process::TokioProcessRunner,
                        ),
                        database_selector: &database_selector,
                    },
                    tenant,
                    arguments.fresh,
                )
                .await;
            progress.finish();
            let ready = result?;
            print!("{}", cli::pull::render_complete(&style, &ready));
            Ok(())
        }
        Commands::Cache(arguments) => {
            let service = application::CacheService::new(ConfigRepository::discover()?);
            match arguments.command {
                CacheCommands::List => {
                    print!("{}", cli::cache::render_list_start(&style));
                    std::io::stdout().flush().map_err(AppError::Output)?;
                    let report = service.list()?;
                    print!("{}", cli::cache::render_list(&style, &report));
                }
                CacheCommands::Clean => {
                    let report = service.clean()?;
                    print!("{}", cli::cache::render_clean(&style, report));
                }
                CacheCommands::Purge(arguments) => {
                    let tenant = TenantLookup::try_from(arguments.tenant)?;
                    let ready = service.purge(&tenant)?;
                    print!("{}", cli::cache::render_purge(&style, &ready));
                }
            }
            Ok(())
        }
    }
}
