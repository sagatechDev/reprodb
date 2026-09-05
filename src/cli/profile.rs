use crate::{
    application::{ProfileRemoval, ProfileSummary},
    cli::output::OutputStyle,
    domain::ProfileName,
};

pub fn render_list(style: &OutputStyle, profiles: &[ProfileSummary]) -> String {
    if profiles.is_empty() {
        return format!(
            "{}\n\n  reprodb profile add salt-source\n",
            style.attention("No source profiles configured. Add the first one with:")
        );
    }

    let mut output = format!("{}\n\n", style.brand("Source profiles"));
    for profile in profiles {
        let marker = if profile.active {
            style.selected("›")
        } else {
            " ".to_owned()
        };
        let active = if profile.active {
            format!(" {}", style.selected("[active]"))
        } else {
            String::new()
        };
        let policy = if profile.production {
            style.attention("production")
        } else {
            style.muted("development")
        };
        output.push_str(&format!(
            "{marker} {}{active}\n    {}:{} · MySQL {} · {policy}\n",
            profile.name, profile.host, profile.port, profile.mysql_series,
        ));
    }
    output
}

pub fn render_activated(style: &OutputStyle, name: &ProfileName) -> String {
    format!(
        "{} Source profile {} is now active.\n",
        style.success("✓"),
        style.value(name.as_str())
    )
}

pub fn render_removal_cancelled(style: &OutputStyle) -> String {
    format!("{} No changes made.\n", style.muted("—"))
}

pub fn render_removed(style: &OutputStyle, name: &ProfileName, removal: ProfileRemoval) -> String {
    let mut output = format!(
        "{} Source profile {} was removed.\n",
        style.success("✓"),
        style.value(name.as_str())
    );
    if removal.credential_was_missing {
        output.push_str(&format!(
            "{} Its credential was already absent from the OS credential store.\n",
            style.attention("!")
        ));
    }
    if removal.was_active {
        output.push_str(&format!(
            "{} No source profile is active. Select one with `reprodb profile use NAME`.\n",
            style.attention("!")
        ));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(name: &str, active: bool, production: bool) -> ProfileSummary {
        ProfileSummary {
            name: ProfileName::try_from(name).unwrap(),
            active,
            host: "127.0.0.1".to_owned(),
            port: 3306,
            mysql_series: "8.4".to_owned(),
            production,
        }
    }

    #[test]
    fn empty_list_includes_the_next_command() {
        let output = render_list(&OutputStyle::plain(), &[]);

        assert!(output.contains("No source profiles configured"));
        assert!(output.contains("reprodb profile add salt-source"));
    }

    #[test]
    fn list_marks_active_and_production_without_relying_on_color() {
        let output = render_list(
            &OutputStyle::plain(),
            &[
                summary("local", true, false),
                summary("production", false, true),
            ],
        );

        assert!(output.contains("› local"));
        assert!(output.contains("active"));
        assert!(output.contains("production"));
        assert!(output.contains("development"));
    }

    #[test]
    fn active_profile_removal_explains_the_required_next_step() {
        let output = render_removed(
            &OutputStyle::plain(),
            &ProfileName::try_from("local").unwrap(),
            ProfileRemoval {
                was_active: true,
                credential_was_missing: true,
            },
        );

        assert!(output.contains("✓ Source profile local was removed"));
        assert!(output.contains("credential was already absent"));
        assert!(output.contains("reprodb profile use NAME"));
    }
}
