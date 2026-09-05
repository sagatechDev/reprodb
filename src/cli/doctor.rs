use crate::{
    application::{DoctorCheck, DoctorReport, DoctorSection, DoctorStatus},
    cli::output::OutputStyle,
};

const SECTIONS: [(DoctorSection, &str); 5] = [
    (DoctorSection::Configuration, "Configuration"),
    (DoctorSection::Storage, "Storage"),
    (DoctorSection::Docker, "Docker"),
    (DoctorSection::Source, "Source"),
    (DoctorSection::Target, "Local target"),
];

pub fn render_start(style: &OutputStyle) -> String {
    format!(
        "{} doctor\n{} Checking configuration, credentials, Docker and MySQL...\n",
        style.brand("reprodb"),
        style.attention("○")
    )
}

pub fn render(style: &OutputStyle, report: &DoctorReport) -> String {
    let mut output = String::new();
    for (section, title) in SECTIONS {
        let checks = report
            .checks
            .iter()
            .filter(|check| check.section == section)
            .collect::<Vec<_>>();
        if checks.is_empty() {
            continue;
        }

        output.push_str(&format!("\n{}\n", style.section(title)));
        for check in checks {
            render_check(&mut output, style, check);
        }
    }

    if report.is_ready() {
        output.push_str(&format!(
            "\n{} Environment ready for reprodb operations.\n",
            style.success("✓")
        ));
    } else {
        let failures = report
            .checks
            .iter()
            .filter(|check| check.status == DoctorStatus::Failed)
            .count();
        output.push_str(&format!(
            "\n{} {failures} required check(s) failed.\n",
            style.danger("✗")
        ));
    }
    output
}

fn render_check(output: &mut String, style: &OutputStyle, check: &DoctorCheck) {
    let symbol = match check.status {
        DoctorStatus::Passed => style.success("✓"),
        DoctorStatus::Warning => style.attention("!"),
        DoctorStatus::Failed => style.danger("✗"),
        DoctorStatus::Skipped => style.muted("–"),
    };
    output.push_str(&format!(
        "  {symbol} {}\n      {}\n",
        check.label, check.detail
    ));
    if let Some(action) = check.action {
        output.push_str(&format!("      {} {action}\n", style.selected("→")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::{DoctorCheck, DoctorFailureKind};

    #[test]
    fn failed_report_is_actionable_without_relying_on_color() {
        let report = DoctorReport {
            checks: vec![DoctorCheck {
                section: DoctorSection::Target,
                label: "target container identity",
                status: DoctorStatus::Failed,
                detail: "the configured name and full ID no longer match".to_owned(),
                action: Some("Run `reprodb setup` before any restore."),
                failure_kind: Some(DoctorFailureKind::Docker),
            }],
        };

        let output = render(&OutputStyle::plain(), &report);

        assert!(output.contains("Local target"));
        assert!(output.contains("✗ target container identity"));
        assert!(output.contains("→ Run `reprodb setup`"));
        assert!(output.contains("1 required check(s) failed"));
    }

    #[test]
    fn ready_report_uses_green_semantics() {
        let report = DoctorReport {
            checks: vec![DoctorCheck {
                section: DoctorSection::Docker,
                label: "Docker",
                status: DoctorStatus::Passed,
                detail: "local context is available".to_owned(),
                action: None,
                failure_kind: None,
            }],
        };

        let output = render(&OutputStyle::colored(), &report);

        assert!(output.contains("\u{1b}[1;32m✓\u{1b}[0m"));
        assert!(output.contains("Environment ready"));
    }

    #[test]
    fn start_message_explains_slow_external_checks() {
        let output = render_start(&OutputStyle::plain());

        assert_eq!(
            output,
            "reprodb doctor\n○ Checking configuration, credentials, Docker and MySQL...\n"
        );
    }
}
