use std::io::Write as _;

use crate::{
    application::{
        PushDumpChoice, PushPlan, PushProgress, PushProgressObserver, PushReady,
        PushSelectionError, PushSelector, RemoteProfileChoice, RestoreDumpChoice,
    },
    cli::{
        cache::format_duration,
        dump::{CliDumpProgress, format_bytes},
        output::OutputStyle,
        prompt,
    },
    domain::{DatabaseName, DumpId, ProfileName},
    infrastructure::compression::{CompressionProgress, CompressionProgressObserver},
};

pub struct CliPushSelector {
    style: OutputStyle,
    profile: Option<ProfileName>,
    dump_id: Option<DumpId>,
    fresh: bool,
    database: Option<DatabaseName>,
    yes: bool,
    interactive: bool,
    now_unix_seconds: u64,
}

impl CliPushSelector {
    pub fn new(
        style: OutputStyle,
        profile: Option<ProfileName>,
        dump_id: Option<DumpId>,
        fresh: bool,
        database: Option<DatabaseName>,
        yes: bool,
    ) -> Self {
        Self {
            style,
            profile,
            dump_id,
            fresh,
            database,
            yes,
            interactive: prompt::is_interactive(),
            now_unix_seconds: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs()),
        }
    }
}

impl PushSelector for CliPushSelector {
    fn profile(&self, choices: &[RemoteProfileChoice]) -> Result<ProfileName, PushSelectionError> {
        // Asked first, so an unattended run without --yes stops before any dump.
        if !self.yes && !self.interactive {
            return Err(PushSelectionError::Unavailable(
                "push writes to another server and there is no terminal to confirm on; rerun with --yes"
                    .to_owned(),
            ));
        }
        // The gate re-checks any name and explains the exact refusal
        // (production, never allowed, protected endpoint, unknown).
        if let Some(requested) = &self.profile {
            return Ok(requested.clone());
        }
        if !self.interactive {
            return Err(PushSelectionError::Unavailable(format!(
                "no terminal to choose the destination on; rerun with --profile (one of: {})",
                choices
                    .iter()
                    .map(|choice| choice.profile.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        let labels = choices
            .iter()
            .map(|choice| format!("{}  {}:{}", choice.profile, choice.host, choice.port))
            .collect::<Vec<_>>();
        let index = prompt::select_push_profile(&labels)
            .map_err(|error| PushSelectionError::Unavailable(error.to_string()))?;
        Ok(choices[index].profile.clone())
    }

    fn dump(
        &self,
        _database: &DatabaseName,
        choices: &[RestoreDumpChoice],
    ) -> Result<PushDumpChoice, PushSelectionError> {
        if self.fresh {
            return Ok(PushDumpChoice::Fresh);
        }
        if let Some(dump_id) = self.dump_id {
            return choices
                .iter()
                .any(|choice| choice.dump_id == dump_id)
                .then_some(PushDumpChoice::Existing(dump_id))
                .ok_or(PushSelectionError::UnknownDump(dump_id));
        }
        if !self.interactive {
            return Err(PushSelectionError::Unavailable(
                "no terminal to choose a dump on; rerun with --fresh or --dump-id ID (see `reprodb cache list`)"
                    .to_owned(),
            ));
        }
        let mut labels = vec!["Generate a new dump now (active profile)".to_owned()];
        labels.extend(
            choices
                .iter()
                .map(|choice| render_dump_choice(self.now_unix_seconds, choice)),
        );
        let index = prompt::select_push_dump(&labels)
            .map_err(|error| PushSelectionError::Unavailable(error.to_string()))?;
        Ok(match index {
            0 => PushDumpChoice::Fresh,
            index => PushDumpChoice::Existing(choices[index - 1].dump_id),
        })
    }

    fn database(&self, source_database: &DatabaseName) -> Result<DatabaseName, PushSelectionError> {
        if let Some(database) = &self.database {
            return Ok(database.clone());
        }
        if !self.interactive {
            return Ok(source_database.clone());
        }
        prompt::select_push_database(source_database)
            .map_err(|error| PushSelectionError::Unavailable(error.to_string()))
    }

    fn confirm(&self, plan: &PushPlan) -> Result<bool, PushSelectionError> {
        print!("{}", render_plan(&self.style, plan));
        let _ = std::io::stdout().flush();
        if self.yes {
            return Ok(true);
        }
        if !self.interactive {
            return Err(PushSelectionError::Unavailable(
                "push writes to another server and there is no terminal to confirm on; rerun with --yes"
                    .to_owned(),
            ));
        }
        prompt::confirm_push_destination(&confirmation_phrase(plan))
            .map_err(|error| PushSelectionError::Unavailable(error.to_string()))
    }
}

/// The user must retype both halves: a right database on the wrong profile
/// is exactly the mistake this command must not allow.
fn confirmation_phrase(plan: &PushPlan) -> String {
    format!("{}/{}", plan.destination_profile, plan.database)
}

fn render_dump_choice(now_unix_seconds: u64, choice: &RestoreDumpChoice) -> String {
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
        "{} push\n{} Validating the destination profile...\n",
        style.brand("reprodb"),
        style.attention("○"),
    )
}

pub fn render_plan(style: &OutputStyle, plan: &PushPlan) -> String {
    format!(
        "\n{}\n  Source:       {} / {}\n  Dump ID:      {}\n  Destination:  {} ({}@{}:{}) / {}\n  MySQL:        {} -> {}\n\n{} Tables present in the dump will be replaced in the remote database.\n",
        style.section("Push plan"),
        style.value(plan.source_profile.as_str()),
        style.value(plan.source_database.as_str()),
        style.value(&plan.dump_id.to_string()),
        style.danger(plan.destination_profile.as_str()),
        plan.destination_user,
        plan.destination_host,
        plan.destination_port,
        style.danger(plan.database.as_str()),
        plan.source_version,
        plan.destination_version,
        style.attention("!"),
    )
}

pub fn render_complete(style: &OutputStyle, ready: &PushReady) -> String {
    format!(
        "\n{} Push complete\n\n  Destination:  {} / {}\n  Imported:     {}\n  Dump ID:      {}\n",
        style.success("✓"),
        style.value(ready.plan.destination_profile.as_str()),
        style.value(ready.plan.database.as_str()),
        format_bytes(ready.imported_bytes),
        style.value(&ready.plan.dump_id.to_string()),
    )
}

pub fn render_cancelled(style: &OutputStyle) -> String {
    format!(
        "\n{} Confirmation did not match; nothing was written.\n",
        style.attention("!")
    )
}

pub struct CliPushProgress {
    style: OutputStyle,
    dump: CliDumpProgress,
}

impl CliPushProgress {
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

impl PushProgressObserver for CliPushProgress {
    fn update(&self, progress: &PushProgress) {
        match progress {
            PushProgress::CreatingDump => println!(
                "{} Exporting a new dump from the active profile...",
                self.style.attention("○")
            ),
            PushProgress::DumpReady(dump_id) => {
                self.dump.finish();
                println!(
                    "{} Managed dump ready · {}",
                    self.style.success("✓"),
                    self.style.muted(&dump_id.to_string()),
                );
            }
            PushProgress::Importing => println!(
                "{} Creating the database if missing and streaming the validated dump...",
                self.style.attention("○")
            ),
        }
        let _ = std::io::stdout().flush();
    }
}

impl CompressionProgressObserver for CliPushProgress {
    fn set_estimated_input_bytes(&self, estimated_input_bytes: u64) {
        self.dump.set_estimated_input_bytes(estimated_input_bytes);
    }

    fn update(&self, progress: CompressionProgress) {
        self.dump.update(progress);
    }
}

#[cfg(test)]
mod tests {
    use crate::domain::MysqlVersion;

    use super::*;

    fn selector() -> CliPushSelector {
        CliPushSelector {
            style: OutputStyle::plain(),
            profile: None,
            dump_id: None,
            fresh: false,
            database: None,
            yes: false,
            interactive: false,
            now_unix_seconds: 1_000,
        }
    }

    fn sandbox() -> Vec<RemoteProfileChoice> {
        vec![RemoteProfileChoice {
            profile: ProfileName::try_from("sandbox").unwrap(),
            host: "sandbox.db.internal".to_owned(),
            port: 3306,
        }]
    }

    fn plan() -> PushPlan {
        PushPlan {
            source_profile: ProfileName::try_from("prod-source").unwrap(),
            source_database: DatabaseName::try_from("salt_sagatec").unwrap(),
            dump_id: DumpId::new(),
            source_version: "8.0.45".parse::<MysqlVersion>().unwrap(),
            destination_profile: ProfileName::try_from("sandbox").unwrap(),
            destination_host: "sandbox.db.internal".to_owned(),
            destination_port: 3306,
            destination_user: "sandbox_writer".to_owned(),
            database: DatabaseName::try_from("salt_sagatec_qa").unwrap(),
            destination_version: "8.4.4".parse::<MysqlVersion>().unwrap(),
        }
    }

    #[test]
    fn without_a_terminal_the_profile_and_confirmation_must_come_from_flags() {
        let unattended = selector().profile(&sandbox()).unwrap_err();
        let profile = CliPushSelector {
            yes: true,
            ..selector()
        }
        .profile(&sandbox())
        .unwrap_err();
        let confirm = selector().confirm(&plan()).unwrap_err();
        let dump = selector()
            .dump(&DatabaseName::try_from("salt_sagatec").unwrap(), &[])
            .unwrap_err();

        assert!(unattended.to_string().contains("--yes"));
        assert!(profile.to_string().contains("--profile"));
        assert!(profile.to_string().contains("sandbox"));
        assert!(confirm.to_string().contains("--yes"));
        assert!(dump.to_string().contains("--fresh"));
    }

    #[test]
    fn a_profile_flag_is_handed_to_the_gate_even_when_not_offered() {
        let requested = CliPushSelector {
            profile: Some(ProfileName::try_from("prod-source").unwrap()),
            yes: true,
            ..selector()
        }
        .profile(&sandbox())
        .unwrap();

        assert_eq!(requested.as_str(), "prod-source");
    }

    #[test]
    fn a_dump_id_of_another_database_is_refused_before_connecting() {
        let dump_id = DumpId::new();
        let error = CliPushSelector {
            dump_id: Some(dump_id),
            ..selector()
        }
        .dump(&DatabaseName::try_from("salt_sagatec").unwrap(), &[])
        .unwrap_err();

        assert!(matches!(error, PushSelectionError::UnknownDump(id) if id == dump_id));
    }

    #[test]
    fn flags_answer_without_prompting() {
        let selector = CliPushSelector {
            profile: Some(ProfileName::try_from("sandbox").unwrap()),
            fresh: true,
            database: Some(DatabaseName::try_from("salt_sagatec_qa").unwrap()),
            yes: true,
            ..selector()
        };
        let source = DatabaseName::try_from("salt_sagatec").unwrap();

        assert_eq!(selector.profile(&sandbox()).unwrap().as_str(), "sandbox");
        assert_eq!(selector.dump(&source, &[]).unwrap(), PushDumpChoice::Fresh);
        assert_eq!(
            selector.database(&source).unwrap().as_str(),
            "salt_sagatec_qa"
        );
        assert!(selector.confirm(&plan()).unwrap());
    }

    #[test]
    fn the_plan_names_source_destination_and_the_overwrite_scope() {
        let output = render_plan(&OutputStyle::plain(), &plan());

        assert!(output.contains("Source:       prod-source / salt_sagatec"));
        assert!(output.contains(
            "Destination:  sandbox (sandbox_writer@sandbox.db.internal:3306) / salt_sagatec_qa"
        ));
        assert_eq!(confirmation_phrase(&plan()), "sandbox/salt_sagatec_qa");
        assert!(output.contains("MySQL:        8.0.45 -> 8.4.4"));
        assert!(output.contains("Tables present in the dump will be replaced"));
        assert!(!output.to_ascii_lowercase().contains("password"));
    }
}
