use std::io::{IsTerminal as _, Write as _};

use crate::{
    application::{
        PullCacheUse, PullDatabaseSelectionError, PullDatabaseSelector, PullProgress,
        PullProgressObserver, PullReady, PullTargetChoice, PullTargetSelectionError,
        PullTargetSelector, RestoreProgress, RestoreProgressObserver,
    },
    cli::{dump::CliDumpProgress, output::OutputStyle, restore},
    infrastructure::{
        cache::CacheMissReason,
        compression::{CompressionProgress, CompressionProgressObserver},
    },
};

use crate::domain::{ContainerName, DatabaseName};

pub struct CliPullTargetSelector {
    requested: Option<ContainerName>,
    interactive: bool,
}

impl CliPullTargetSelector {
    pub fn new(requested: Option<ContainerName>) -> Self {
        Self {
            requested,
            interactive: std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        }
    }
}

impl PullTargetSelector for CliPullTargetSelector {
    fn select(
        &self,
        choices: &[PullTargetChoice],
    ) -> Result<ContainerName, PullTargetSelectionError> {
        if let Some(requested) = &self.requested {
            return choices
                .iter()
                .find(|choice| choice.container == *requested)
                .map(|choice| choice.container.clone())
                .ok_or(PullTargetSelectionError);
        }
        let default = choices
            .iter()
            .find(|choice| choice.is_default)
            .or_else(|| choices.first())
            .ok_or(PullTargetSelectionError)?;
        if !self.interactive || choices.len() == 1 {
            return Ok(default.container.clone());
        }
        crate::cli::prompt::select_pull_target(choices).map_err(|_| PullTargetSelectionError)
    }
}

pub struct CliPullDatabaseSelector {
    requested: Option<DatabaseName>,
    interactive: bool,
}

impl CliPullDatabaseSelector {
    pub fn new(requested: Option<DatabaseName>) -> Self {
        Self {
            requested,
            interactive: std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        }
    }
}

impl PullDatabaseSelector for CliPullDatabaseSelector {
    fn select(
        &self,
        source_database: &DatabaseName,
    ) -> Result<DatabaseName, PullDatabaseSelectionError> {
        if let Some(requested) = &self.requested {
            return Ok(requested.clone());
        }
        if !self.interactive {
            return Ok(source_database.clone());
        }
        crate::cli::prompt::select_pull_database(source_database)
            .map_err(|_| PullDatabaseSelectionError)
    }
}

pub fn render_start(style: &OutputStyle) -> String {
    format!("{} pull\n", style.brand("reprodb"))
}

pub fn render_complete(style: &OutputStyle, ready: &PullReady) -> String {
    let cache = match ready.cache {
        PullCacheUse::Hit { age_seconds } => {
            format!("reused ({})", format_age(age_seconds))
        }
        PullCacheUse::Created => "new dump".to_owned(),
    };
    let plan = &ready.restored.plan;
    let timing = ready.metrics.dump.map_or_else(
        || {
            format!(
                "restore {} · total {}",
                crate::cli::dump::format_duration(ready.metrics.restore_elapsed),
                crate::cli::dump::format_duration(ready.metrics.total_elapsed),
            )
        },
        |dump| {
            format!(
                "dump {} · restore {} · total {}",
                crate::cli::dump::format_duration(dump.elapsed),
                crate::cli::dump::format_duration(ready.metrics.restore_elapsed),
                crate::cli::dump::format_duration(ready.metrics.total_elapsed),
            )
        },
    );
    format!(
        "\n{} Tenant ready\n\n  Database:   {}\n  Container:  {}\n  Domain:     {}\n  Cache:      {}\n  Timing:     {}\n  Dump ID:    {}\n",
        style.success("✓"),
        style.value(plan.database.as_str()),
        style.value(plan.container.as_str()),
        style.value(plan.local_domain.as_str()),
        cache,
        timing,
        style.value(&plan.dump_id.to_string()),
    )
}

pub struct CliPullProgress {
    style: OutputStyle,
    dump: CliDumpProgress,
}

impl CliPullProgress {
    pub fn new(style: OutputStyle) -> Self {
        Self {
            style,
            dump: CliDumpProgress::new(style),
        }
    }

    pub fn finish(&self) {
        self.dump.finish();
    }
}

impl PullProgressObserver for CliPullProgress {
    fn update(&self, progress: PullProgress) {
        match progress {
            selected @ PullProgress::SourceSelected { .. } => {
                print!("{}", render_source_selected(&self.style, &selected));
            }
            PullProgress::CheckingCache => {
                println!("{} Checking the local cache...", self.style.attention("○"));
            }
            PullProgress::CacheHit {
                dump_id,
                age_seconds,
            } => println!(
                "{} Cached dump found · age {} · {}",
                self.style.success("✓"),
                format_age(age_seconds),
                self.style.muted(&dump_id.to_string()),
            ),
            PullProgress::CacheMiss(reason) => println!(
                "{} {}",
                self.style
                    .attention(if reason == CacheMissReason::FreshRequested {
                        "!"
                    } else {
                        "○"
                    }),
                cache_miss_message(reason),
            ),
            PullProgress::CreatingDump => {
                println!(
                    "{} Resolving the tenant and exporting the source database...",
                    self.style.attention("○")
                );
                println!(
                    "{} Avoid schema migrations on the source until the dump finishes.",
                    self.style.attention("!")
                );
            }
            PullProgress::DumpReady(dump_id) => {
                self.dump.finish();
                println!(
                    "{} Managed dump ready · {}",
                    self.style.success("✓"),
                    self.style.muted(&dump_id.to_string()),
                );
            }
        }
        let _ = std::io::stdout().flush();
    }
}

pub fn render_source_selected(style: &OutputStyle, progress: &PullProgress) -> String {
    match progress {
        PullProgress::SourceSelected {
            profile,
            production: true,
        } => format!(
            "{} PRODUCTION SOURCE · {}\n{} Cache is mandatory; restore remains restricted to the attested local target.\n",
            style.danger("!"),
            style.danger(profile.as_str()),
            style.attention("!"),
        ),
        PullProgress::SourceSelected {
            profile,
            production: false,
        } => format!(
            "{} Source profile: {} · development\n",
            style.selected("›"),
            style.value(profile.as_str())
        ),
        _ => String::new(),
    }
}

impl CompressionProgressObserver for CliPullProgress {
    fn set_estimated_input_bytes(&self, estimated_input_bytes: u64) {
        self.dump.set_estimated_input_bytes(estimated_input_bytes);
    }

    fn update(&self, progress: CompressionProgress) {
        self.dump.update(progress);
    }
}

impl RestoreProgressObserver for CliPullProgress {
    fn update(&self, progress: &RestoreProgress) {
        match progress {
            RestoreProgress::PlanReady(plan) => {
                print!("{}", restore::render_plan(&self.style, plan));
            }
            RestoreProgress::RestoringDatabase => println!(
                "{} Recreating the database and streaming the validated dump...",
                self.style.attention("○")
            ),
            RestoreProgress::RegisteringTenant => {
                println!("{} Database import completed", self.style.success("✓"));
                println!(
                    "{} Updating the local Salt tenant registration...",
                    self.style.attention("○")
                );
            }
        }
        let _ = std::io::stdout().flush();
    }
}

fn cache_miss_message(reason: CacheMissReason) -> &'static str {
    match reason {
        CacheMissReason::FreshRequested => "Fresh dump requested; the local cache will be ignored.",
        CacheMissReason::NotFound => "No matching cached dump was found.",
        CacheMissReason::Expired => "The matching cached dump has expired.",
        CacheMissReason::SourceChanged => "The source profile changed since the cached dump.",
        CacheMissReason::PolicyChanged => "The dump policy changed since the cached dump.",
        CacheMissReason::InUse => "The matching cached dump is currently in use.",
        CacheMissReason::CorruptMetadata
        | CacheMissReason::IdentityChanged
        | CacheMissReason::ClockInFuture
        | CacheMissReason::SizeMismatch
        | CacheMissReason::ChecksumMismatch => "The matching cached dump is not reusable.",
    }
}

fn format_age(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 60 * 60 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h {}m", seconds / 3_600, (seconds % 3_600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::{
        application::{PullCacheUse, PullDumpMetrics, PullMetrics, RestorePlan, RestoreReady},
        domain::{
            ContainerName, DatabaseName, DomainAlias, DumpId, MysqlVersion, ProfileName, TenantId,
            TenantLookup,
        },
    };

    use super::*;

    fn ready(cache: PullCacheUse) -> PullReady {
        PullReady {
            cache,
            restored: RestoreReady {
                plan: RestorePlan {
                    profile: ProfileName::try_from("salt-local").unwrap(),
                    tenant_lookup: TenantLookup::try_from("sagatec").unwrap(),
                    tenant_id: TenantId::try_from("salt_sagatec").unwrap(),
                    source_database: DatabaseName::try_from("salt_sagatec").unwrap(),
                    database: DatabaseName::try_from("salt_sagatec").unwrap(),
                    dump_id: DumpId::new(),
                    source_version: "8.4.4".parse::<MysqlVersion>().unwrap(),
                    client_version: "8.4.4".parse::<MysqlVersion>().unwrap(),
                    container: ContainerName::try_from("mysql-8").unwrap(),
                    local_domain: DomainAlias::try_from("sagatec").unwrap(),
                },
                imported_bytes: 1024,
            },
            metrics: PullMetrics {
                dump: (cache == PullCacheUse::Created).then_some(PullDumpMetrics {
                    elapsed: Duration::from_secs(4),
                    uncompressed_bytes: 2_000_000,
                    compressed_bytes: 1_000_000,
                }),
                restore_elapsed: Duration::from_secs(3),
                total_elapsed: Duration::from_secs(8),
            },
        }
    }

    #[test]
    fn completion_distinguishes_reused_and_new_cache_entries() {
        let reused = render_complete(
            &OutputStyle::plain(),
            &ready(PullCacheUse::Hit { age_seconds: 2_100 }),
        );
        let created = render_complete(&OutputStyle::plain(), &ready(PullCacheUse::Created));

        assert!(reused.contains("Cache:      reused (35m)"));
        assert!(reused.contains("Timing:     restore 00:03 · total 00:08"));
        assert!(created.contains("Cache:      new dump"));
        assert!(created.contains("Timing:     dump 00:04 · restore 00:03 · total 00:08"));
        assert!(reused.contains("✓ Tenant ready"));
        assert!(!reused.to_ascii_lowercase().contains("password"));
    }

    #[test]
    fn cache_messages_are_actionable_without_exposing_internal_errors() {
        assert_eq!(
            cache_miss_message(CacheMissReason::FreshRequested),
            "Fresh dump requested; the local cache will be ignored."
        );
        assert_eq!(
            cache_miss_message(CacheMissReason::ChecksumMismatch),
            "The matching cached dump is not reusable."
        );
        assert_eq!(format_age(34), "34s");
        assert_eq!(format_age(2_100), "35m");
        assert_eq!(format_age(7_500), "2h 5m");
    }

    #[test]
    fn production_source_warning_states_cache_and_local_restore_guards() {
        let progress = PullProgress::SourceSelected {
            profile: ProfileName::try_from("salt-production").unwrap(),
            production: true,
        };
        let output = render_source_selected(&OutputStyle::plain(), &progress);

        assert!(output.contains("PRODUCTION SOURCE · salt-production"));
        assert!(output.contains("Cache is mandatory"));
        assert!(output.contains("attested local target"));
    }

    #[test]
    fn database_selection_uses_an_explicit_name_or_the_source_default_non_interactively() {
        let source = DatabaseName::try_from("salt_sagatec").unwrap();
        let default = CliPullDatabaseSelector {
            requested: None,
            interactive: false,
        };
        let custom = CliPullDatabaseSelector {
            requested: Some(DatabaseName::try_from("salt_sagatec_debug").unwrap()),
            interactive: false,
        };

        assert_eq!(default.select(&source).unwrap(), source);
        assert_eq!(
            custom.select(&source).unwrap().as_str(),
            "salt_sagatec_debug"
        );
    }

    #[test]
    fn target_selection_uses_an_explicit_container_or_the_default_non_interactively() {
        let choices = vec![
            PullTargetChoice {
                container: ContainerName::try_from("mysql-source").unwrap(),
                is_default: false,
            },
            PullTargetChoice {
                container: ContainerName::try_from("mysql-8").unwrap(),
                is_default: true,
            },
        ];
        let requested = CliPullTargetSelector {
            requested: Some(ContainerName::try_from("mysql-source").unwrap()),
            interactive: false,
        };
        let default = CliPullTargetSelector {
            requested: None,
            interactive: false,
        };

        assert_eq!(requested.select(&choices).unwrap().as_str(), "mysql-source");
        assert_eq!(default.select(&choices).unwrap().as_str(), "mysql-8");
        assert!(
            CliPullTargetSelector {
                requested: Some(ContainerName::try_from("mysql-unknown").unwrap()),
                interactive: false,
            }
            .select(&choices)
            .is_err()
        );
    }
}
