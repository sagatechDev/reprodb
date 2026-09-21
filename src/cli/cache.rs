use crate::{
    application::{CacheListEntry, CacheListReport, CacheListStatus},
    cli::output::OutputStyle,
    domain::DatabaseName,
    infrastructure::cache_cleanup::{CacheSweepReport, PrunePlan, PruneReport},
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

pub fn render_sweep(style: &OutputStyle, report: CacheSweepReport) -> String {
    let removed = report
        .orphan_partials_removed
        .saturating_add(report.interrupted_deletions_removed);
    format!(
        "{} {} item{} of cache garbage removed · abandoned partials {} · interrupted deletions {}\n",
        style.success("✓"),
        removed,
        if removed == 1 { "" } else { "s" },
        report.orphan_partials_removed,
        report.interrupted_deletions_removed,
    )
}

pub fn render_prune_plan(
    style: &OutputStyle,
    plan: &PrunePlan,
    database: Option<&DatabaseName>,
    criterion: &str,
) -> String {
    let scope = database.map_or_else(
        || "every database".to_owned(),
        |database| database.as_str().to_owned(),
    );
    let mut output = format!(
        "{} cache prune\n\n  Scope:     {}\n  Criterion: {}\n",
        style.brand("reprodb"),
        style.value(&scope),
        style.value(criterion),
    );
    if plan.is_empty() {
        output.push_str(&format!(
            "\n{} No managed dump matches. Nothing to remove.\n",
            style.muted("—")
        ));
        return output;
    }
    output.push_str(&format!(
        "\n{} managed dump{} · {} to reclaim\n",
        plan.selected.len(),
        if plan.selected.len() == 1 { "" } else { "s" },
        format_bytes(plan.total_compressed_bytes()),
    ));
    for candidate in &plan.selected {
        output.push_str(&format!(
            "  {}  {} / {} · {}\n",
            style.muted(&candidate.dump_id.to_string()),
            candidate.profile,
            candidate.database.as_str(),
            format_bytes(candidate.compressed_bytes),
        ));
    }
    output.push_str(&format!(
        "\n{} Removal is permanent; these dumps cannot be restored afterwards.\n",
        style.attention("!"),
    ));
    output
}

pub fn render_prune_result(style: &OutputStyle, report: PruneReport) -> String {
    let mut output = format!(
        "\n{} {} managed dump{} removed · {} reclaimed\n",
        style.success("✓"),
        report.artifacts_removed,
        if report.artifacts_removed == 1 {
            ""
        } else {
            "s"
        },
        format_bytes(report.bytes_removed),
    );
    if report.locked_entries_skipped > 0 {
        output.push_str(&format!(
            "{} {} dump{} in use and kept.\n",
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
            "{} {} unreadable entr{} kept; inspect them with `reprodb cache list`.\n",
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

pub fn render_prune_cancelled(style: &OutputStyle) -> String {
    format!("\n{} Cancelled. Nothing was removed.\n", style.muted("—"))
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
        "\n{}  {}\n  Profile: {} · Age: {} · Fresh for pull: {} · Size: {}\n  Dump ID: {}\n",
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
        CacheListStatus::Ready => style.success("✓ fresh"),
        CacheListStatus::Stale => style.selected("• stale"),
        CacheListStatus::Prunable => style.attention("! prunable"),
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

/// Renders a duration the way the user typed it: `7d`, not `7d 0h`.
pub fn format_duration_compact(seconds: u64) -> String {
    for (unit, size) in [("d", 86_400), ("h", 3_600), ("m", 60)] {
        if seconds >= size && seconds.is_multiple_of(size) {
            return format!("{}{unit}", seconds / size);
        }
    }
    format!("{seconds}s")
}

pub fn format_duration(seconds: u64) -> String {
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

pub fn format_bytes(bytes: u64) -> String {
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

    use crate::domain::{DatabaseName, DumpId, ProfileName, PruneCandidate};

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
        assert!(output.contains("✓ fresh  acme_production"));
        assert!(output.contains("Profile: local-source"));
        assert!(output.contains("Age: 1m"));
        assert!(output.contains("Fresh for pull: in 1m"));
        assert!(output.contains("Size: 1.5 MiB"));
        assert!(output.contains("Dump ID:"));
    }

    fn candidate(database: &str, compressed_bytes: u64) -> PruneCandidate {
        PruneCandidate {
            profile: ProfileName::try_from("local-source").unwrap(),
            database: DatabaseName::try_from(database).unwrap(),
            dump_id: DumpId::new(),
            completed_at_unix_seconds: 9_000,
            compressed_bytes,
        }
    }

    #[test]
    fn a_stale_dump_is_reported_as_restorable_rather_than_expired() {
        let report = CacheListReport {
            cache_root: PathBuf::from("/Users/dev/.reprodb/cache"),
            now_unix_seconds: 10_000,
            total_compressed_bytes: 1_572_864,
            entries: vec![entry(CacheListStatus::Stale)],
        };

        let output = render_list(&OutputStyle::plain(), &report);

        assert!(output.contains("• stale"));
        assert!(!output.to_ascii_lowercase().contains("expired"));
    }

    #[test]
    fn the_sweep_report_never_mentions_removing_a_complete_dump() {
        let output = render_sweep(
            &OutputStyle::plain(),
            CacheSweepReport {
                orphan_partials_removed: 2,
                interrupted_deletions_removed: 1,
                ..CacheSweepReport::default()
            },
        );

        assert!(output.contains("3 items of cache garbage removed"));
        assert!(output.contains("abandoned partials 2"));
        assert!(!output.to_ascii_lowercase().contains("managed dump"));
    }

    #[test]
    fn the_prune_plan_states_the_scope_size_and_that_removal_is_permanent() {
        let plan = PrunePlan {
            selected: vec![
                candidate("acme_production", 1_572_864),
                candidate("acme_production", 524_288),
            ],
            invalid_entries_skipped: 0,
        };

        let output = render_prune_plan(
            &OutputStyle::plain(),
            &plan,
            Some(&DatabaseName::try_from("acme_production").unwrap()),
            "older than 7d",
        );

        assert!(output.contains("Scope:     acme_production"));
        assert!(output.contains("Criterion: older than 7d"));
        assert!(output.contains("2 managed dumps · 2.0 MiB to reclaim"));
        assert!(output.contains("Removal is permanent"));
    }

    #[test]
    fn an_empty_prune_plan_promises_nothing_will_be_removed() {
        let output = render_prune_plan(
            &OutputStyle::plain(),
            &PrunePlan::default(),
            None,
            "older than 7d",
        );

        assert!(output.contains("Scope:     every database"));
        assert!(output.contains("Nothing to remove"));
        assert!(!output.contains("Removal is permanent"));
    }

    #[test]
    fn the_prune_result_is_explicit_about_dumps_kept_because_they_are_in_use() {
        let output = render_prune_result(
            &OutputStyle::plain(),
            PruneReport {
                artifacts_removed: 1,
                bytes_removed: 1_572_864,
                locked_entries_skipped: 1,
                invalid_entries_skipped: 0,
            },
        );

        assert!(output.contains("1 managed dump removed · 1.5 MiB reclaimed"));
        assert!(output.contains("1 dump is in use and kept"));
    }
}
