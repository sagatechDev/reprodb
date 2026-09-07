use std::{
    io::{self, IsTerminal, Write},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
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
const ETA_SAMPLE_WINDOW: Duration = Duration::from_secs(2);

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
    estimated_input_bytes: AtomicU64,
    last_rendered: Mutex<Duration>,
}

impl CliDumpProgress {
    pub fn new(style: OutputStyle) -> Self {
        Self {
            style,
            enabled: io::stderr().is_terminal(),
            rendered: AtomicBool::new(false),
            estimated_input_bytes: AtomicU64::new(0),
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
    fn set_estimated_input_bytes(&self, estimated_input_bytes: u64) {
        self.estimated_input_bytes
            .store(estimated_input_bytes, Ordering::Relaxed);
    }

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
        let eta = estimated_remaining(progress, self.estimated_input_bytes.load(Ordering::Relaxed))
            .map(|duration| format!(" | ETA ~{}", format_duration(duration)))
            .unwrap_or_default();
        eprint!(
            "\r{} Exporting: {} | {}/s | {}{}",
            self.style.attention("◌"),
            format_bytes(progress.input_bytes),
            format_bytes(progress.input_bytes_per_second() as u64),
            format_duration(progress.elapsed),
            eta,
        );
        let _ = io::stderr().flush();
    }
}

fn estimated_remaining(progress: CompressionProgress, estimated_total: u64) -> Option<Duration> {
    if progress.elapsed < ETA_SAMPLE_WINDOW
        || progress.input_bytes == 0
        || progress.input_bytes >= estimated_total
    {
        return None;
    }
    let bytes_per_second = progress.input_bytes_per_second();
    if !bytes_per_second.is_finite() || bytes_per_second <= 0.0 {
        return None;
    }
    let remaining_bytes = estimated_total - progress.input_bytes;
    Some(Duration::from_secs_f64(
        remaining_bytes as f64 / bytes_per_second,
    ))
}

pub(crate) fn format_bytes(bytes: u64) -> String {
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

    #[test]
    fn eta_waits_for_a_sample_and_disappears_after_the_estimate_is_reached() {
        assert_eq!(
            estimated_remaining(
                CompressionProgress {
                    input_bytes: 2_000,
                    elapsed: Duration::from_secs(2),
                },
                10_000,
            ),
            Some(Duration::from_secs(8))
        );
        assert!(
            estimated_remaining(
                CompressionProgress {
                    input_bytes: 1_000,
                    elapsed: Duration::from_millis(500),
                },
                10_000,
            )
            .is_none()
        );
        assert!(
            estimated_remaining(
                CompressionProgress {
                    input_bytes: 10_001,
                    elapsed: Duration::from_secs(3),
                },
                10_000,
            )
            .is_none()
        );
    }
}
