//! Application library for the `reprodb` CLI.
//!
//! Command parsing and application services live in this library so they can
//! be tested without spawning the compiled executable. `main.rs` remains the
//! composition root for process-level concerns.

use std::future::Future;
use std::io::Write as _;

pub mod application;
pub mod cli;
pub mod domain;
pub mod error;
pub mod infrastructure;

use cli::output::OutputStyle;
use cli::{CacheCommands, DatabaseCommands, ProfileCommands};
pub use cli::{Cli, Commands};
use domain::{DatabaseName, ProfileName};
pub use error::{AppError, ErrorCategory};
use infrastructure::config::ConfigRepository;

pub async fn execute(cli: Cli) -> Result<(), AppError> {
    execute_with_cancellation(
        cli,
        infrastructure::cancellation::CancellationToken::default(),
    )
    .await
}

pub async fn execute_until_ctrl_c(cli: Cli) -> Result<(), AppError> {
    let cancellation = infrastructure::cancellation::CancellationToken::default();
    let execution = execute_with_cancellation(cli, cancellation.clone());
    supervise_execution(execution, cancellation, tokio::signal::ctrl_c()).await
}

async fn supervise_execution<Execution, Interrupt>(
    execution: Execution,
    cancellation: infrastructure::cancellation::CancellationToken,
    interrupt: Interrupt,
) -> Result<(), AppError>
where
    Execution: Future<Output = Result<(), AppError>>,
    Interrupt: Future<Output = std::io::Result<()>>,
{
    tokio::pin!(execution);
    tokio::pin!(interrupt);
    tokio::select! {
        result = &mut execution => result,
        signal = &mut interrupt => match signal {
            Ok(()) => {
                cancellation.cancel();
                let _ = execution.await;
                Err(AppError::Interrupted)
            }
            Err(error) => {
                tracing::warn!(%error, "could not listen for Ctrl+C");
                execution.await
            }
        }
    }
}

pub async fn execute_with_cancellation(
    cli: Cli,
    cancellation: infrastructure::cancellation::CancellationToken,
) -> Result<(), AppError> {
    tracing::debug!(command = cli.command.name(), "command received");
    let style = OutputStyle::stdout(cli.color);
    let credential_root = infrastructure::config::AppPaths::discover()?
        .config_dir()
        .join("credentials");
    let credential_store =
        infrastructure::credentials::RuntimeCredentialStore::discover(credential_root);

    match cli.command {
        Commands::Setup(arguments) if arguments.preview => {
            print!("{}", cli::preview::setup(&style));
            Ok(())
        }
        Commands::Setup(arguments) => {
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

            let selected = if arguments.non_interactive {
                let target = arguments.target.as_deref().ok_or(
                    cli::prompt::PromptError::IncompleteNonInteractive("--target is required"),
                )?;
                candidates
                    .iter()
                    .position(|candidate| candidate.name.as_str() == target)
                    .ok_or(cli::prompt::PromptError::NonInteractiveTargetNotFound)?
            } else {
                cli::prompt::select_local_target(&candidates)?
            };
            let candidate = &candidates[selected];
            if candidate.state != infrastructure::docker::ContainerState::Running {
                if arguments.non_interactive {
                    return Err(cli::prompt::PromptError::NonInteractiveTargetNotRunning.into());
                }
                if !cli::prompt::confirm_container_start(candidate)? {
                    print!("{}", cli::setup::render_cancelled(&style));
                    return Ok(());
                }
                print!("{}", cli::setup::render_starting(&style, candidate));
                discovery
                    .start_container(&docker_context, &candidate.id)
                    .await?;
            }
            if service.has_target_named(&candidate.name)? {
                if arguments.non_interactive && !arguments.yes {
                    return Err(cli::prompt::PromptError::IncompleteNonInteractive(
                        "--yes is required to replace an existing target",
                    )
                    .into());
                }
                if !arguments.non_interactive
                    && !cli::prompt::confirm_target_replacement(candidate)?
                {
                    print!("{}", cli::setup::render_cancelled(&style));
                    return Ok(());
                }
            }

            let input = if arguments.non_interactive {
                cli::prompt::collect_local_target_non_interactive(
                    docker_context,
                    candidate,
                    &arguments,
                )?
            } else {
                cli::prompt::collect_local_target(docker_context, candidate)?
            };
            println!("\n{}", cli::setup::render_verifying(&style, candidate));
            let verifier = infrastructure::mysql::DockerLocalTargetVerifier::new(
                infrastructure::process::TokioProcessRunner,
            );
            let configured = service
                .configure(&credential_store, &verifier, input)
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
                let name = ProfileName::try_from(arguments.name.clone())?;
                let service = application::ProfileService::new(ConfigRepository::discover()?);
                service.ensure_name_available(&name)?;
                println!("{}", cli::profile::render_add_intro(&style, &name));
                let input = if arguments.non_interactive {
                    cli::prompt::collect_new_profile_non_interactive(name, &arguments)?
                } else {
                    cli::prompt::collect_new_profile(name)?
                };
                println!(
                    "\n{}",
                    cli::profile::render_verifying(&style, input.tls_mode)
                );
                let verifier = infrastructure::mysql::DockerSourceProfileVerifier::new(
                    infrastructure::process::TokioProcessRunner,
                );
                let created = service.add(&credential_store, &verifier, input).await?;
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
                let removal = service.remove(&credential_store, &name).await?;
                print!("{}", cli::profile::render_removed(&style, &name, removal));
                Ok(())
            }
        },
        Commands::Doctor(arguments) if arguments.preview => {
            print!("{}", cli::preview::doctor(&style));
            Ok(())
        }
        Commands::Database(arguments) => match arguments.command {
            DatabaseCommands::List(arguments) => {
                print!("{}", cli::database::render_start(&style));
                std::io::stdout().flush().map_err(AppError::Output)?;
                let service =
                    application::DatabaseCatalogService::new(ConfigRepository::discover()?);
                let reader = infrastructure::mysql::DockerDatabaseCatalogReader::new(
                    infrastructure::process::TokioProcessRunner,
                );
                let page = service
                    .list(&credential_store, &reader, arguments.limit)
                    .await?;
                print!("{}", cli::database::render_page(&style, &page));
                Ok(())
            }
        },
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
                .run(&credential_store, &docker, &storage, &source, &target)
                .await;
            print!("{}", cli::doctor::render(&style, &report));
            if let Some(kind) = report.first_failure_kind() {
                return Err(AppError::DoctorChecksFailed { kind });
            }
            Ok(())
        }
        Commands::Dump(arguments) => {
            let database = DatabaseName::try_from(arguments.database)?;
            print!("{}", cli::dump::render_start(&style));
            std::io::stdout().flush().map_err(AppError::Output)?;
            let repository = ConfigRepository::discover()?;
            let workflow = infrastructure::mysql::DockerDumpWorkflow::new(
                infrastructure::process::TokioProcessRunner,
            );
            let progress = std::sync::Arc::new(cli::dump::CliDumpProgress::new(style));
            let service = application::DumpService::new(repository)
                .with_progress(progress.clone())
                .with_status(progress.clone());
            let executor =
                infrastructure::mysql::DockerMysqlDumpExecutor::new(cancellation.clone());
            let result = service
                .create(&credential_store, &workflow, &executor, database)
                .await;
            progress.finish();
            let created = result?;
            print!("{}", cli::dump::render_complete(&style, &created));
            Ok(())
        }
        Commands::Restore(arguments) => {
            let database = DatabaseName::try_from(arguments.database)?;
            let dump_id = arguments
                .dump_id
                .map(|raw| raw.parse::<domain::DumpId>())
                .transpose()?;
            let target_container = arguments
                .target
                .map(domain::ContainerName::try_from)
                .transpose()?;
            let target_database = arguments
                .target_database
                .map(domain::DatabaseName::try_from)
                .transpose()?;
            print!("{}", cli::restore::render_start(&style));
            std::io::stdout().flush().map_err(AppError::Output)?;

            let repository = ConfigRepository::discover()?;
            let progress = std::sync::Arc::new(cli::restore::CliRestoreProgress::new(style));
            let service = application::RestoreService::new(repository).with_progress(progress);
            let executor =
                infrastructure::mysql::DockerMysqlRestoreExecutor::new(cancellation.clone());
            let restored = service
                .restore_to(
                    &credential_store,
                    &infrastructure::mysql::DockerLocalTargetAttestor::new(
                        infrastructure::process::TokioProcessRunner,
                    ),
                    executor,
                    &cli::restore::CliRestoreDumpSelector::new(),
                    application::RestoreRequest {
                        database,
                        dump_id,
                        target_container,
                        target_database,
                    },
                )
                .await?;
            print!("{}", cli::restore::render_complete(&style, &restored));
            Ok(())
        }
        Commands::Pull(arguments) if arguments.preview => {
            let database = DatabaseName::try_from(arguments.database)?;
            let target_container = arguments
                .target
                .map(domain::ContainerName::try_from)
                .transpose()?;
            let target_database = arguments
                .target_database
                .map(domain::DatabaseName::try_from)
                .transpose()?;
            print!(
                "{}",
                cli::preview::pull(
                    &style,
                    &database,
                    arguments.fresh,
                    target_container.as_ref(),
                    target_database.as_ref(),
                )
            );
            Ok(())
        }
        Commands::Pull(arguments) => {
            let database = DatabaseName::try_from(arguments.database)?;
            let target_container = arguments
                .target
                .map(domain::ContainerName::try_from)
                .transpose()?;
            let target_database = arguments
                .target_database
                .map(domain::DatabaseName::try_from)
                .transpose()?;
            let database_selector = cli::pull::CliPullDatabaseSelector::new(target_database);
            let target_selector = cli::pull::CliPullTargetSelector::new(target_container);
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
            let dump_executor =
                infrastructure::mysql::DockerMysqlDumpExecutor::new(cancellation.clone());
            let restore_executor =
                infrastructure::mysql::DockerMysqlRestoreExecutor::new(cancellation.clone());
            let result = service
                .pull(
                    &credential_store,
                    application::PullDumpDependencies {
                        preflight: &workflow,
                        executor: &dump_executor,
                    },
                    application::PullRestoreDependencies {
                        target_attestor: &infrastructure::mysql::DockerLocalTargetAttestor::new(
                            infrastructure::process::TokioProcessRunner,
                        ),
                        executor: restore_executor,
                        target_selector: &target_selector,
                        database_selector: &database_selector,
                    },
                    database,
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
                CacheCommands::Prune(arguments) => {
                    let database = arguments.database.map(DatabaseName::try_from).transpose()?;
                    let (selection, criterion) = match (
                        arguments.all,
                        arguments.keep_last,
                        arguments.older_than.as_deref(),
                    ) {
                        (true, _, _) => (domain::PruneSelection::All, "every dump".to_owned()),
                        (_, Some(count), _) => (
                            domain::PruneSelection::KeepLast { count },
                            format!("keep the newest {count} per profile and database"),
                        ),
                        (_, _, Some(raw)) => {
                            let seconds = cli::parse_duration_seconds(raw)?;
                            (
                                domain::PruneSelection::OlderThan { seconds },
                                format!("older than {raw}"),
                            )
                        }
                        _ => (
                            domain::PruneSelection::default(),
                            format!(
                                "older than {} (default retention)",
                                cli::cache::format_duration_compact(
                                    domain::DEFAULT_RETENTION_SECONDS
                                )
                            ),
                        ),
                    };

                    let plan = service.plan_prune(selection, database.as_ref())?;
                    print!(
                        "{}",
                        cli::cache::render_prune_plan(&style, &plan, database.as_ref(), &criterion)
                    );
                    std::io::stdout().flush().map_err(AppError::Output)?;
                    if plan.is_empty() {
                        return Ok(());
                    }
                    if !arguments.yes {
                        if !cli::prompt::is_interactive() {
                            return Err(AppError::NonInteractivePrune);
                        }
                        let reclaimed = cli::cache::format_bytes(plan.total_compressed_bytes());
                        if !cli::prompt::confirm_prune(plan.selected.len(), &reclaimed)? {
                            print!("{}", cli::cache::render_prune_cancelled(&style));
                            return Ok(());
                        }
                    }
                    let report = service.execute_prune(&plan)?;
                    print!("{}", cli::cache::render_prune_result(&style, report));
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod cancellation_tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use super::*;

    #[tokio::test]
    async fn ctrl_c_waits_for_cooperative_cleanup_and_returns_130() {
        let cancellation = infrastructure::cancellation::CancellationToken::default();
        let operation_token = cancellation.clone();
        let cleaned_up = Arc::new(AtomicBool::new(false));
        let operation_cleanup = Arc::clone(&cleaned_up);
        let execution = async move {
            operation_token.cancelled().await;
            tokio::task::yield_now().await;
            operation_cleanup.store(true, Ordering::Release);
            Err(AppError::Interrupted)
        };

        let error = supervise_execution(execution, cancellation, async { Ok(()) })
            .await
            .unwrap_err();

        assert_eq!(error.exit_code(), 130);
        assert!(cleaned_up.load(Ordering::Acquire));
    }
}
