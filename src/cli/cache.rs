use crate::{
    application::{CacheListEntry, CacheListReport, CacheListStatus, CachePurgeReady},
    cli::output::OutputStyle,
    infrastructure::cache_cleanup::CacheCleanupReport,
};

pub fn render_list_start(style: &OutputStyle) -> String {
    format!(
        "{} cache\n\n{} Verifying managed dump metadata, size and checksum...\n",
        style.brand("reprodb"),
        style.attention("○"),
    )
}

pub fn render_list(style: &OutputStyle, report: &CacheListReport) -> String {
    let mut output = format!(
        "\n{}\n  {}\n",
        style.section("Cache directory"),
        report.cache_root.display(),
    );
    if report.entries.is_empty() {
        output.push_str(&format!(
            "\n{} No managed dumps found.\n\n  Create one with: reprodb pull acme\n",
            style.muted("—")
        ));
        return output;
    }

    output.push_str(&format!(
        "\n{} managed dump{} · {} total\n",
        report.entries.len(),
        if report.entries.len() == 1 { "" } else { "s" },
        format_bytes(report.total_compressed_bytes),
    ));
    for entry in &report.entries {
        output.push_str(&render_entry(style, entry, report.now_unix_seconds));
    }
    output
}

pub fn render_clean(style: &OutputStyle, report: CacheCleanupReport) -> String {
    let removed = report
        .expired_artifacts_removed
        .saturating_add(report.orphan_partials_removed)
        .saturating_add(report.interrupted_deletions_removed);
    let mut output = format!(
        "{} cache clean\n\n{} Cleanup complete · {} item{} removed\n",
        style.brand("reprodb"),
        style.success("✓"),
        removed,
        if removed == 1 { "" } else { "s" },
    );
    output.push_str(&format!(
        "  Expired dumps:          {}\n  Abandoned partials:     {}\n  Interrupted deletions:  {}\n",
        report.expired_artifacts_removed,
        report.orphan_partials_removed,
        report.interrupted_deletions_removed,
    ));
    let skipped = report
        .locked_entries_skipped
        .saturating_add(report.future_entries_skipped)
        .saturating_add(report.invalid_entries_skipped);
    if skipped > 0 {
        output.push_str(&format!(
            "{} {} item{} kept for safety · locked {} · future clock {} · invalid {}\n",
            style.attention("!"),
            skipped,
            if skipped == 1 { "" } else { "s" },
            report.locked_entries_skipped,
            report.future_entries_skipped,
            report.invalid_entries_skipped,
        ));
    }
    output
}

pub fn render_purge(style: &OutputStyle, ready: &CachePurgeReady) -> String {
    let report = ready.report;
    let mut output = format!(
        "{} cache purge\n\n  Database: {}\n  Profile:  {}\n\n",
        style.brand("reprodb"),
        style.value(ready.database.as_str()),
        style.value(ready.profile.as_str()),
    );
    if report.artifacts_removed == 0 {
        output.push_str(&format!(
            "{} No matching unlocked dump was removed.\n",
            style.attention("!")
        ));
    } else {
        output.push_str(&format!(
            "{} {} managed dump{} removed.\n",
            style.success("✓"),
            report.artifacts_removed,
            if report.artifacts_removed == 1 {
                " was"
            } else {
                "s were"
            },
        ));
    }
    if report.locked_entries_skipped > 0 {
        output.push_str(&format!(
            "{} {} dump{} currently in use and kept.\n",
            style.attention("!"),
            report.locked_entries_skipped,
            if report.locked_entries_skipped == 1 {
                " is"
            } else {
                "s are"
            },
        ));
    }
    if report.invalid_entries_skipped > 0 {
        output.push_str(&format!(
            "{} {} unreadable entr{} kept because its tenant could not be proven.\n",
            style.attention("!"),
            report.invalid_entries_skipped,
            if report.invalid_entries_skipped == 1 {
                "y was"
            } else {
                "ies were"
            },
        ));
    }
    output
}

fn render_entry(style: &OutputStyle, entry: &CacheListEntry, now: u64) -> String {
    let database = entry.database.as_str();
    let age = entry
        .completed_at_unix_seconds
        .map(|completed| relative_age(now, completed))
        .unwrap_or_else(|| "unknown".to_owned());
    let expiration = entry
        .expires_at_unix_seconds
        .map(|expires| relative_expiration(now, expires))
        .unwrap_or_else(|| "unknown".to_owned());
    let size = entry
        .compressed_bytes
        .map(format_bytes)
        .unwrap_or_else(|| "unknown".to_owned());
    format!(
        "\n{}  {}\n  Profile: {} · Age: {} · Expires: {} · Size: {}\n  Dump ID: {}\n",
        render_status(style, entry.status),
        style.value(database),
        entry.profile,
        age,
        expiration,
        size,
        style.muted(&entry.dump_id.to_string()),
    )
}

fn render_status(style: &OutputStyle, status: CacheListStatus) -> String {
    match status {
        CacheListStatus::Ready => style.success("✓ ready"),
        CacheListStatus::Expired => style.attention("! expired"),
        CacheListStatus::InUse => style.selected("• in use"),
        CacheListStatus::ProfileMissing => style.attention("! profile missing"),
        CacheListStatus::SourceChanged => style.attention("! source changed"),
        CacheListStatus::PolicyChanged => style.attention("! policy changed"),
        CacheListStatus::ClockInFuture => style.danger("× future clock"),
        CacheListStatus::CorruptMetadata => style.danger("× bad metadata"),
        CacheListStatus::IdentityChanged => style.danger("× identity mismatch"),
        CacheListStatus::MissingFile => style.danger("× missing file"),
        CacheListStatus::SizeMismatch => style.danger("× size mismatch"),
        CacheListStatus::ChecksumMismatch => style.danger("× checksum mismatch"),
    }
}

fn relative_age(now: u64, completed: u64) -> String {
    if completed > now {
        "from the future".to_owned()
    } else {
        format_duration(now - completed)
    }
}

fn relative_expiration(now: u64, expires: u64) -> String {
    if expires > now {
        format!("in {}", format_duration(expires - now))
    } else {
        format!("{} ago", format_duration(now - expires))
    }
}

fn format_duration(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h {}m", seconds / 3_600, (seconds % 3_600) / 60)
    } else {
        format!("{}d {}h", seconds / 86_400, (seconds % 86_400) / 3_600)
    }
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::domain::{DatabaseName, DumpId, ProfileName};
    use crate::infrastructure::cache_cleanup::CachePurgeReport;

    use super::*;

    fn entry(status: CacheListStatus) -> CacheListEntry {
        CacheListEntry {
            profile: ProfileName::try_from("local-source").unwrap(),
            dump_id: DumpId::new(),
            database: DatabaseName::try_from("acme_production").unwrap(),
            completed_at_unix_seconds: Some(9_900),
            expires_at_unix_seconds: Some(10_100),
            compressed_bytes: Some(1_572_864),
            status,
        }
    }

    #[test]
    fn list_exposes_location_identity_freshness_size_and_integrity() {
        let report = CacheListReport {
            cache_root: PathBuf::from("/Users/dev/.reprodb/cache"),
            now_unix_seconds: 10_000,
            total_compressed_bytes: 1_572_864,
            entries: vec![entry(CacheListStatus::Ready)],
        };

        let output = format!(
            "{}{}",
            render_list_start(&OutputStyle::plain()),
            render_list(&OutputStyle::plain(), &report)
        );

        assert!(output.contains("/Users/dev/.reprodb/cache"));
        assert!(output.contains("✓ ready  acme_production"));
        assert!(output.contains("Profile: local-source"));
        assert!(output.contains("Age: 1m"));
        assert!(output.contains("Expires: in 1m"));
        assert!(output.contains("Size: 1.5 MiB"));
        assert!(output.contains("Dump ID:"));
    }

    #[test]
    fn cleanup_and_purge_reports_are_explicit_about_kept_entries() {
        let clean = render_clean(
            &OutputStyle::plain(),
            CacheCleanupReport {
                expired_artifacts_removed: 2,
                locked_entries_skipped: 1,
                invalid_entries_skipped: 1,
                ..CacheCleanupReport::default()
            },
        );
        assert!(clean.contains("2 items removed"));
        assert!(clean.contains("2 items kept for safety"));

        let purge = render_purge(
            &OutputStyle::plain(),
            &CachePurgeReady {
                profile: ProfileName::try_from("local-source").unwrap(),
                database: DatabaseName::try_from("acme_production").unwrap(),
                report: CachePurgeReport {
                    artifacts_removed: 1,
                    locked_entries_skipped: 1,
                    invalid_entries_skipped: 0,
                },
            },
        );
        assert!(purge.contains("Database: acme_production"));
        assert!(purge.contains("Profile:  local-source"));
        assert!(purge.contains("1 managed dump was removed"));
        assert!(purge.contains("1 dump is currently in use and kept"));
    }
}
