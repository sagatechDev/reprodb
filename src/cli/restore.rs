use std::io::Write as _;

use crate::{
    application::{
        RestoreDumpChoice, RestoreDumpSelector, RestorePlan, RestoreProgress,
        RestoreProgressObserver, RestoreReady, RestoreSelectionError,
    },
    cli::{cache::format_duration, dump::format_bytes, output::OutputStyle, prompt},
    domain::{DatabaseName, DumpId},
};

/// Asks the user which stored dump to restore.
///
/// Always asks, even when a single dump exists: restore drops and recreates the
/// local database, so the choice stays explicit on every run.
pub struct CliRestoreDumpSelector {
    now_unix_seconds: u64,
}

impl CliRestoreDumpSelector {
    pub const fn at(now_unix_seconds: u64) -> Self {
        Self { now_unix_seconds }
    }

    /// Ages are cosmetic here, so a clock that cannot be read falls back to the
    /// epoch and simply renders every dump as "from the future".
    pub fn new() -> Self {
        Self::at(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs()),
        )
    }
}

impl Default for CliRestoreDumpSelector {
    fn default() -> Self {
        Self::new()
    }
}

impl RestoreDumpSelector for CliRestoreDumpSelector {
    fn select(
        &self,
        database: &DatabaseName,
        choices: &[RestoreDumpChoice],
    ) -> Result<DumpId, RestoreSelectionError> {
        if choices.is_empty() {
            return Err(RestoreSelectionError::NoCandidates {
                database: database.clone(),
            });
        }
        if !prompt::is_interactive() {
            return Err(RestoreSelectionError::Unavailable(format!(
                "no terminal to select a dump on; rerun with one of: {}",
                choices
                    .iter()
                    .map(|choice| choice.dump_id.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        let labels = choices
            .iter()
            .map(|choice| render_choice(self.now_unix_seconds, choice))
            .collect::<Vec<_>>();
        let index = prompt::select_restore_dump(&labels)
            .map_err(|error| RestoreSelectionError::Unavailable(error.to_string()))?;
        Ok(choices[index].dump_id)
    }
}

fn render_choice(now_unix_seconds: u64, choice: &RestoreDumpChoice) -> String {
    let age = if choice.completed_at_unix_seconds > now_unix_seconds {
        "from the future".to_owned()
    } else {
        format!(
            "{} ago",
            format_duration(now_unix_seconds - choice.completed_at_unix_seconds)
        )
    };
    format!(
        "{}  {:>14}  {:>10}  {}  MySQL {}",
        choice.dump_id,
        age,
        format_bytes(choice.compressed_bytes),
        choice.profile,
        choice.source_version,
    )
}

pub fn render_start(style: &OutputStyle) -> String {
    format!(
        "{} restore\n{} Locating the managed dump and validating the local target...\n",
        style.brand("reprodb"),
        style.attention("○"),
    )
}

pub fn render_plan(style: &OutputStyle, plan: &RestorePlan) -> String {
    format!(
        "\n{}\n  Source:     {} / {}\n  Dump ID:    {}\n  MySQL:      {} (client {})\n  Target:     {}/{}\n\n{} The selected local target database will be replaced.\n",
        style.section("Restore plan"),
        style.value(plan.profile.as_str()),
        style.value(plan.source_database.as_str()),
        style.value(&plan.dump_id.to_string()),
        plan.source_version,
        plan.client_version,
        style.value(plan.container.as_str()),
        style.value(plan.database.as_str()),
        style.attention("!"),
    )
}

pub fn render_complete(style: &OutputStyle, restored: &RestoreReady) -> String {
    let plan = &restored.plan;
    format!(
        "\n{} Restore ready\n\n  Database:   {}\n  Container:  {}\n  Imported:   {}\n  Dump ID:    {}\n",
        style.success("✓"),
        style.value(plan.database.as_str()),
        style.value(plan.container.as_str()),
        format_bytes(restored.imported_bytes),
        style.value(&plan.dump_id.to_string()),
    )
}

pub struct CliRestoreProgress {
    style: OutputStyle,
}

impl CliRestoreProgress {
    pub const fn new(style: OutputStyle) -> Self {
        Self { style }
    }
}

impl RestoreProgressObserver for CliRestoreProgress {
    fn update(&self, progress: &RestoreProgress) {
        match progress {
            RestoreProgress::PlanReady(plan) => print!("{}", render_plan(&self.style, plan)),
            RestoreProgress::RestoringDatabase => println!(
                "{} Recreating the database and streaming the validated dump...",
                self.style.attention("○")
            ),
        }
        let _ = std::io::stdout().flush();
    }
}

#[cfg(test)]
mod tests {
    use crate::domain::{ContainerName, DatabaseName, DumpId, MysqlVersion, ProfileName};

    use super::*;

    fn plan() -> RestorePlan {
        RestorePlan {
            profile: ProfileName::try_from("local-source").unwrap(),
            source_database: DatabaseName::try_from("acme_production").unwrap(),
            database: DatabaseName::try_from("acme_production").unwrap(),
            dump_id: DumpId::new(),
            source_version: "8.4.4".parse::<MysqlVersion>().unwrap(),
            client_version: "8.4.4".parse::<MysqlVersion>().unwrap(),
            container: ContainerName::try_from("mysql-8").unwrap(),
        }
    }

    #[test]
    fn plan_makes_the_destructive_local_scope_visible() {
        let output = render_plan(&OutputStyle::plain(), &plan());

        assert!(output.contains("Source:     local-source / acme_production"));
        assert!(output.contains("Target:     mysql-8/acme_production"));
        assert!(output.contains("selected local target database will be replaced"));
        assert!(!output.to_ascii_lowercase().contains("password"));
    }

    #[test]
    fn completion_reports_only_the_restored_database_identity() {
        let restored = RestoreReady {
            plan: plan(),
            imported_bytes: 5 * 1024 * 1024,
        };

        let output = render_complete(&OutputStyle::plain(), &restored);

        assert!(output.contains("✓ Restore ready"));
        assert!(!output.contains("Domain:"));
        assert!(output.contains("Imported:   5.0 MiB"));
    }
}
