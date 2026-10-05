use std::{
    io::{IsTerminal as _, Write as _},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::{
    application::{
        PushDumpChoice, PushPlan, PushProgress, PushProgressObserver, PushReady,
        PushSelectionError, PushSelector, RemoteProfileChoice, RestoreDumpChoice,
    },
    cli::{
        cache::format_duration,
        dump::{CliDumpProgress, format_bytes, format_duration as format_clock},
        output::OutputStyle,
        prompt,
    },
    domain::{DatabaseName, DumpId, MysqlVersion, ProfileName},
    infrastructure::{
        compression::{CompressionProgress, CompressionProgressObserver},
        mysql::{ImportProgress, ImportProgressObserver},
    },
};

pub struct CliPushSelector {
    style: OutputStyle,
    profile: Option<ProfileName>,
    dump_id: Option<DumpId>,
    fresh: bool,
    database: Option<DatabaseName>,
    yes: bool,
    allow_downgrade: bool,
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
        allow_downgrade: bool,
    ) -> Self {
        Self {
            style,
            profile,
            dump_id,
            fresh,
            database,
            yes,
            allow_downgrade,
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
            .map(render_profile_choice)
            .collect::<Vec<_>>();
        let index = prompt::select_push_profile(&labels)
            .map_err(|error| PushSelectionError::Unavailable(error.to_string()))?;
        Ok(choices[index].profile.clone())
    }

    fn allow_push(&self, choice: &RemoteProfileChoice) -> Result<bool, PushSelectionError> {
        if !self.interactive {
            return Err(PushSelectionError::Unavailable(format!(
                "profile `{0}` has never received a push and there is no terminal to allow it on; run `reprodb profile allow-push {0}`",
                choice.profile
            )));
        }
        prompt::confirm_allow_push(
            choice.profile.as_str(),
            &format!("{}:{}", choice.host, choice.port),
        )
        .map_err(|error| PushSelectionError::Unavailable(error.to_string()))
    }

    fn accept_downgrade(
        &self,
        dump: MysqlVersion,
        destination: MysqlVersion,
    ) -> Result<bool, PushSelectionError> {
        if self.allow_downgrade {
            return Ok(true);
        }
        if !self.interactive {
            return Err(PushSelectionError::Unavailable(format!(
                "the dump comes from MySQL {dump} and the destination runs {destination}; there is no terminal to confirm the downgrade on, rerun with --allow-downgrade"
            )));
        }
        prompt::confirm_downgrade(&dump.to_string(), &destination.to_string())
            .map_err(|error| PushSelectionError::Unavailable(error.to_string()))
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

fn render_profile_choice(choice: &RemoteProfileChoice) -> String {
    format!(
        "{}  {}:{}{}",
        choice.profile,
        choice.host,
        choice.port,
        if choice.accepts_push {
            ""
        } else {
            "  (never received a push; you will be asked to allow it)"
        }
    )
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
    ) + &downgrade_warning(style, plan)
}

fn downgrade_warning(style: &OutputStyle, plan: &PushPlan) -> String {
    if plan.downgrade {
        format!(
            "{} Downgrade: importing a MySQL {} dump into {}. Features newer than {} may fail to import.\n",
            style.danger("!"),
            plan.source_version,
            plan.destination_version,
            plan.destination_version,
        )
    } else {
        String::new()
    }
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
        "\n{} Push cancelled; nothing was written.\n",
        style.attention("!")
    )
}

const IMPORT_RENDER_INTERVAL: Duration = Duration::from_millis(250);
const IMPORT_ETA_SAMPLE_WINDOW: Duration = Duration::from_secs(3);
const WAITING_TICK: Duration = Duration::from_millis(500);
const BAR_WIDTH: usize = 24;

pub struct CliPushProgress {
    style: OutputStyle,
    dump: CliDumpProgress,
    enabled: bool,
    rendered: AtomicBool,
    last_rendered: Mutex<Option<Duration>>,
    waiting: Mutex<Option<WaitingTicker>>,
}

/// Keeps a live clock on screen while MySQL applies the statements that are
/// already sent, so a long final index build never looks like a hang.
struct WaitingTicker {
    stop: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

impl CliPushProgress {
    pub fn new(style: OutputStyle) -> Self {
        Self {
            style,
            dump: CliDumpProgress::new(style),
            enabled: std::io::stderr().is_terminal(),
            rendered: AtomicBool::new(false),
            last_rendered: Mutex::new(None),
            waiting: Mutex::new(None),
        }
    }

    pub fn finish(&self) {
        self.dump.finish();
        let ticker = self
            .waiting
            .lock()
            .ok()
            .and_then(|mut waiting| waiting.take());
        if let Some(ticker) = ticker {
            ticker.stop.store(true, Ordering::Relaxed);
            let _ = ticker.handle.join();
        }
        if self.enabled && self.rendered.swap(false, Ordering::Relaxed) {
            eprintln!();
        }
    }

    fn draw(&self, line: &str) {
        self.rendered.store(true, Ordering::Relaxed);
        eprint!("\r\x1b[2K{line}");
        let _ = std::io::stderr().flush();
    }
}

impl ImportProgressObserver for CliPushProgress {
    fn update(&self, progress: ImportProgress) {
        if !self.enabled {
            return;
        }
        let Ok(mut last_rendered) = self.last_rendered.lock() else {
            return;
        };
        let complete = progress.sent_bytes >= progress.total_bytes;
        if let Some(previous) = *last_rendered
            && progress.elapsed.saturating_sub(previous) < IMPORT_RENDER_INTERVAL
            && !complete
        {
            return;
        }
        *last_rendered = Some(progress.elapsed);
        self.draw(&render_import_line(&self.style, progress));
    }

    fn streaming_finished(&self) {
        if !self.enabled {
            eprintln!("All data sent; waiting for MySQL to apply the last statements...");
            return;
        }
        // Keep the final 100% line and start the waiting clock below it.
        eprintln!();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let style = self.style;
        let started = Instant::now();
        let handle = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                eprint!(
                    "\r\x1b[2K{}",
                    render_waiting_line(&style, started.elapsed())
                );
                let _ = std::io::stderr().flush();
                std::thread::sleep(WAITING_TICK);
            }
        });
        self.rendered.store(true, Ordering::Relaxed);
        if let Ok(mut waiting) = self.waiting.lock() {
            *waiting = Some(WaitingTicker { stop, handle });
        }
    }
}

fn render_import_line(style: &OutputStyle, progress: ImportProgress) -> String {
    let total = progress.total_bytes.max(1);
    let fraction = (progress.sent_bytes as f64 / total as f64).clamp(0.0, 1.0);
    let filled = (fraction * BAR_WIDTH as f64).round() as usize;
    let bar = format!("{}{}", "█".repeat(filled), "░".repeat(BAR_WIDTH - filled));
    let eta = import_eta(progress)
        .map(|remaining| format!(" | ETA ~{}", format_clock(remaining)))
        .unwrap_or_default();
    format!(
        "{} Importing: {} / {} {:>3}% [{bar}] | avg {}/s | {}{eta}",
        style.attention("◌"),
        format_bytes(progress.sent_bytes),
        format_bytes(progress.total_bytes),
        (fraction * 100.0).floor() as u64,
        format_bytes(progress.bytes_per_second() as u64),
        format_clock(progress.elapsed),
    )
}

fn import_eta(progress: ImportProgress) -> Option<Duration> {
    if progress.elapsed < IMPORT_ETA_SAMPLE_WINDOW
        || progress.sent_bytes == 0
        || progress.sent_bytes >= progress.total_bytes
    {
        return None;
    }
    let rate = progress.bytes_per_second();
    if !rate.is_finite() || rate <= 0.0 {
        return None;
    }
    Some(Duration::from_secs_f64(
        (progress.total_bytes - progress.sent_bytes) as f64 / rate,
    ))
}

fn render_waiting_line(style: &OutputStyle, waited: Duration) -> String {
    format!(
        "{} All data sent; waiting for MySQL to apply the last statements · {}",
        style.attention("◌"),
        format_clock(waited),
    )
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
            allow_downgrade: false,
            interactive: false,
            now_unix_seconds: 1_000,
        }
    }

    fn sandbox() -> Vec<RemoteProfileChoice> {
        vec![RemoteProfileChoice {
            profile: ProfileName::try_from("sandbox").unwrap(),
            host: "sandbox.db.internal".to_owned(),
            port: 3306,
            accepts_push: true,
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
            downgrade: false,
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

    #[test]
    fn a_downgrade_is_spelled_out_in_the_plan() {
        let plain = render_plan(&OutputStyle::plain(), &plan());
        let downgraded = render_plan(
            &OutputStyle::plain(),
            &PushPlan {
                source_version: "8.4.8".parse::<MysqlVersion>().unwrap(),
                destination_version: "8.0.43".parse::<MysqlVersion>().unwrap(),
                downgrade: true,
                ..plan()
            },
        );

        assert!(!plain.contains("Downgrade"));
        assert!(downgraded.contains("Downgrade: importing a MySQL 8.4.8 dump into 8.0.43"));
    }

    #[test]
    fn without_a_terminal_consent_and_downgrade_point_to_the_matching_escape_hatch() {
        let choice = RemoteProfileChoice {
            accepts_push: false,
            ..sandbox().remove(0)
        };
        let allow = selector().allow_push(&choice).unwrap_err();
        let downgrade = selector()
            .accept_downgrade("8.4.8".parse().unwrap(), "8.0.43".parse().unwrap())
            .unwrap_err();
        let flagged = CliPushSelector {
            allow_downgrade: true,
            ..selector()
        }
        .accept_downgrade("8.4.8".parse().unwrap(), "8.0.43".parse().unwrap())
        .unwrap();

        assert!(
            allow
                .to_string()
                .contains("reprodb profile allow-push sandbox")
        );
        assert!(downgrade.to_string().contains("--allow-downgrade"));
        assert!(flagged);
    }

    #[test]
    fn the_menu_marks_profiles_that_were_never_allowed() {
        let allowed = render_profile_choice(&sandbox()[0]);
        let pending = render_profile_choice(&RemoteProfileChoice {
            accepts_push: false,
            ..sandbox().remove(0)
        });

        assert_eq!(allowed, "sandbox  sandbox.db.internal:3306");
        assert!(pending.contains("you will be asked to allow it"));
    }

    fn import(sent: u64, total: u64, seconds: u64) -> ImportProgress {
        ImportProgress {
            sent_bytes: sent,
            total_bytes: total,
            elapsed: Duration::from_secs(seconds),
        }
    }

    #[test]
    fn the_import_line_shows_size_percent_bar_rate_and_eta() {
        let gib = 1024 * 1024 * 1024;
        let line = render_import_line(&OutputStyle::plain(), import(gib / 2, 2 * gib, 600));

        assert!(
            line.contains("Importing: 512.0 MiB / 2.0 GiB  25%"),
            "{line}"
        );
        assert!(line.contains("[██████░░░░░░░░░░░░░░░░░░]"), "{line}");
        assert!(line.contains("| 10:00"), "{line}");
        // 1.5 GiB left at the observed rate of 0.5 GiB per 10 minutes.
        assert!(line.contains("ETA ~30:00"), "{line}");
    }

    #[test]
    fn eta_waits_for_a_sample_and_disappears_once_everything_is_sent() {
        assert_eq!(import_eta(import(10, 100, 1)), None);
        assert_eq!(import_eta(import(100, 100, 60)), None);
        assert!(render_import_line(&OutputStyle::plain(), import(100, 100, 60)).contains("100%"));
    }

    #[test]
    fn the_waiting_line_says_what_is_happening_and_keeps_a_clock() {
        let line = render_waiting_line(&OutputStyle::plain(), Duration::from_secs(75));

        assert!(
            line.contains("All data sent; waiting for MySQL to apply the last statements · 01:15")
        );
    }
}
