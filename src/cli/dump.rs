use std::{
    io::{self, IsTerminal, Write},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crate::{
    application::DumpCreated,
    cli::output::OutputStyle,
    domain::DumpPolicyNotice,
    infrastructure::compression::{CompressionProgress, CompressionProgressObserver},
};

const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

pub fn render_start(style: &OutputStyle) -> String {
    format!(
        "{} dump\n{} Resolving tenant and validating MySQL source...\n{} Avoid schema migrations on the source until the dump finishes.\n",
        style.brand("reprodb"),
        style.attention("○"),
        style.attention("!"),
    )
}

pub fn render_complete(style: &OutputStyle, dump: &DumpCreated) -> String {
    let mut output = format!(
        "\n{} Dump ready\n\n  Profile:   {}\n  Tenant:    {}\n  Database:  {}\n  Dump ID:   {}\n  MySQL:     {} (client {})\n  Data:      {} → {}\n  Duration:  {}\n  Cache:     {}\n",
        style.success("✓"),
        style.value(dump.profile.as_str()),
        style.value(dump.tenant_id.as_str()),
        style.value(dump.database.as_str()),
        style.value(&dump.dump_id.to_string()),
        dump.source_version,
        dump.client_version,
        format_bytes(dump.uncompressed_bytes),
        format_bytes(dump.compressed_bytes),
        format_duration(dump.elapsed),
        dump.artifact_path.display(),
    );
    for notice in &dump.notices {
        let detail = match notice {
            DumpPolicyNotice::ConcurrentDdlMustBePrevented => {
                "Avoid schema changes on the source while this dump is running.".to_owned()
            }
            DumpPolicyNotice::DefinerObjectsPresent { count } => format!(
                "{count} view/trigger object(s) contain DEFINER metadata; restore will validate compatibility."
            ),
        };
        output.push_str(&format!("  {} {detail}\n", style.attention("!")));
    }
    output
}

pub struct CliDumpProgress {
    style: OutputStyle,
    enabled: bool,
    rendered: AtomicBool,
    last_rendered: Mutex<Duration>,
}

impl CliDumpProgress {
    pub fn new(style: OutputStyle) -> Self {
        Self {
            style,
            enabled: io::stderr().is_terminal(),
            rendered: AtomicBool::new(false),
            last_rendered: Mutex::new(Duration::ZERO),
        }
    }

    pub fn finish(&self) {
        if self.enabled && self.rendered.swap(false, Ordering::Relaxed) {
            eprintln!();
        }
    }
}

impl CompressionProgressObserver for CliDumpProgress {
    fn update(&self, progress: CompressionProgress) {
        if !self.enabled {
            return;
        }
        let Ok(mut last_rendered) = self.last_rendered.lock() else {
            return;
        };
        if progress.elapsed.saturating_sub(*last_rendered) < PROGRESS_INTERVAL
            && progress.input_bytes > 0
        {
            return;
        }
        *last_rendered = progress.elapsed;
        self.rendered.store(true, Ordering::Relaxed);
        eprint!(
            "\r{} Exporting: {} | {}/s | {}",
            self.style.attention("◌"),
            format_bytes(progress.input_bytes),
            format_bytes(progress.input_bytes_per_second() as u64),
            format_duration(progress.elapsed),
        );
        let _ = io::stderr().flush();
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::domain::{DumpId, MysqlVersion, ProfileName, TenantId, TenantLookup};

    use super::*;

    #[test]
    fn start_and_result_are_readable_without_color_or_secrets() {
        assert_eq!(
            render_start(&OutputStyle::plain()),
            "reprodb dump\n○ Resolving tenant and validating MySQL source...\n! Avoid schema migrations on the source until the dump finishes.\n"
        );
        let tenant = TenantLookup::try_from("sagatec").unwrap();
        let result = DumpCreated {
            dump_id: DumpId::new(),
            artifact_path: PathBuf::from("/cache/profiles/local-source/salt_sagatec/dump"),
            profile: ProfileName::try_from("local-source").unwrap(),
            tenant_lookup: tenant,
            tenant_id: TenantId::try_from("salt_sagatec").unwrap(),
            database: crate::domain::DatabaseName::try_from("salt_sagatec").unwrap(),
            source_version: "8.4.4".parse::<MysqlVersion>().unwrap(),
            client_version: "8.4.4".parse::<MysqlVersion>().unwrap(),
            uncompressed_bytes: 1_572_864,
            compressed_bytes: 524_288,
            elapsed: Duration::from_secs(44),
            notices: vec![DumpPolicyNotice::ConcurrentDdlMustBePrevented],
        };
        let output = render_complete(&OutputStyle::plain(), &result);

        assert!(output.contains("✓ Dump ready"));
        assert!(output.contains("Tenant:    salt_sagatec"));
        assert!(output.contains("Data:      1.5 MiB → 512.0 KiB"));
        assert!(output.contains("Duration:  00:44"));
        assert!(!output.to_ascii_lowercase().contains("password"));
    }

    #[test]
    fn byte_and_time_formatting_have_stable_units() {
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(format_duration(Duration::from_secs(125)), "02:05");
    }
}
