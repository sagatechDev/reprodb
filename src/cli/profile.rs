use crate::{
    application::{ProfileCreated, ProfileRemoval, ProfileSummary},
    cli::output::OutputStyle,
    domain::ProfileName,
};

pub fn render_add_intro(style: &OutputStyle, name: &ProfileName) -> String {
    format!(
        "{} · Add source profile\n\nProfile  {}\n",
        style.brand("reprodb"),
        style.value(name.as_str())
    )
}

pub fn render_verifying(style: &OutputStyle, tls_mode: crate::domain::MysqlTlsMode) -> String {
    format!(
        "{} Detecting Docker context, preparing the approved client and testing the source...\n  TLS mode: {tls_mode}",
        style.selected("›"),
    )
}

pub fn render_created(style: &OutputStyle, created: &ProfileCreated) -> String {
    format!(
        "{ok} Source connection verified: {vendor} {server}\n\
         {ok} Approved MySQL client ready: {client}\n\
         {ok} Docker context: {context}\n\
         {ok} TLS mode: {tls}\n\
         {ok} Password saved in the OS credential store\n\
         {ok} Source profile {name} saved and activated\n",
        ok = style.success("✓"),
        vendor = created.vendor,
        server = created.server_version,
        client = created.client_version,
        context = created.docker_context,
        tls = created.tls_mode,
        name = style.value(created.name.as_str()),
    )
}

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
            "{marker} {}{active}\n    {}:{} · MySQL {} · TLS {} · {policy}\n",
            profile.name, profile.host, profile.port, profile.mysql_series, profile.tls_mode,
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
            tls_mode: crate::domain::MysqlTlsMode::Required,
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
    fn verification_message_makes_the_effective_tls_policy_visible() {
        let output = render_verifying(&OutputStyle::plain(), crate::domain::MysqlTlsMode::Required);

        assert!(output.contains("testing the source"));
        assert!(output.contains("TLS mode: REQUIRED"));
    }

    #[test]
    fn created_profile_output_reports_only_safe_connection_metadata() {
        let client = crate::infrastructure::mysql::ClientCatalog::resolve("8.4").unwrap();
        let output = render_created(
            &OutputStyle::plain(),
            &ProfileCreated {
                name: ProfileName::try_from("salt-source").unwrap(),
                docker_context: "desktop-linux".to_owned(),
                server_version: "8.4.4".parse().unwrap(),
                vendor: "MySQL Community Server".to_owned(),
                client_version: client.version(),
                tls_mode: crate::domain::MysqlTlsMode::Required,
            },
        );

        assert!(output.contains("Source connection verified"));
        assert!(output.contains("TLS mode: REQUIRED"));
        assert!(output.contains("Docker context: desktop-linux"));
        assert!(output.contains("saved and activated"));
        assert!(!output.to_ascii_lowercase().contains("password-that"));
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
