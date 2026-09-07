use crate::{
    application::LocalTargetConfigured,
    cli::output::OutputStyle,
    infrastructure::docker::{
        ContainerHealth, ContainerState, DockerContainerCandidate, PublishedPort,
    },
};

pub fn render_discovered(
    style: &OutputStyle,
    context: &str,
    candidates: &[DockerContainerCandidate],
) -> String {
    let mut output = format!(
        "{} · Local MySQL setup\n\n{}\n  {} Docker context: {}\n\n{}\n",
        style.brand("reprodb"),
        style.section("Docker"),
        style.success("✓"),
        style.value(context),
        style.section("MySQL containers found"),
    );

    if candidates.is_empty() {
        output.push_str(&format!(
            "  {} No container exposing MySQL port 3306 or using a MySQL image was found.\n",
            style.attention("!")
        ));
        return output;
    }

    for candidate in candidates {
        output.push_str(&format!(
            "  {}  {}  {}  {}\n",
            style.value(candidate.name.as_str()),
            candidate.image,
            state_label(candidate.state),
            port_label(&candidate.published_ports),
        ));
        if candidate.health != ContainerHealth::NotConfigured {
            output.push_str(&format!(
                "      health: {}\n",
                health_label(candidate.health)
            ));
        }
        if candidate
            .published_ports
            .iter()
            .any(PublishedPort::is_exposed_on_all_interfaces)
        {
            output.push_str(&format!(
                "      {} Port is exposed on every host interface\n",
                style.attention("!")
            ));
        }
        if candidate.published_ports.is_empty() {
            output.push_str(&format!(
                "      {} Port 3306 is not published to the host\n",
                style.attention("!")
            ));
        }
    }
    output
}

pub fn choice_labels(candidates: &[DockerContainerCandidate]) -> Vec<String> {
    candidates
        .iter()
        .map(|candidate| {
            format!(
                "{} — {} — {} — {}",
                candidate.name,
                candidate.image,
                state_label(candidate.state),
                port_label(&candidate.published_ports),
            )
        })
        .collect()
}

pub fn render_verifying(style: &OutputStyle, candidate: &DockerContainerCandidate) -> String {
    format!(
        "{} Preparing the approved client and testing {} by its exact container ID...\n",
        style.selected("›"),
        style.value(candidate.name.as_str()),
    )
}

pub fn render_starting(style: &OutputStyle, candidate: &DockerContainerCandidate) -> String {
    format!(
        "{} Starting {} and waiting up to 10 seconds for MySQL...\n",
        style.selected("›"),
        style.value(candidate.name.as_str()),
    )
}

pub fn render_configured(style: &OutputStyle, configured: &LocalTargetConfigured) -> String {
    let mut output = format!(
        "{ok} Target connection verified: {vendor} {version}\n\
         {ok} Docker context: {context}\n\
         {ok} Tenant database allowlist: {prefix}*\n\
         {ok} Target trust: {trust}\n\
         {ok} Password saved in the OS credential store\n\
         {ok} Local target {container} saved\n",
        ok = style.success("✓"),
        vendor = configured.vendor,
        version = configured.server_version,
        context = configured.docker_context,
        prefix = configured.tenant_database_prefix,
        trust = if configured.managed_by_reprodb {
            "reprodb-managed container"
        } else {
            "existing container confirmed during setup"
        },
        container = style.value(configured.container_name.as_str()),
    );
    if configured.replaced_existing {
        output.push_str(&format!(
            "{} Existing target configuration replaced; other saved targets were preserved.\n",
            style.attention("!")
        ));
    } else {
        output.push_str(&format!(
            "{} This is now the default target; other saved targets were preserved.\n",
            style.selected("›")
        ));
    }
    if configured.previous_credential_was_missing {
        output.push_str(&format!(
            "{} The previous credential was already absent.\n",
            style.attention("!")
        ));
    }
    output
}

pub fn render_cancelled(style: &OutputStyle) -> String {
    format!("{} No changes made.\n", style.muted("—"))
}

fn state_label(state: ContainerState) -> &'static str {
    match state {
        ContainerState::Running => "running",
        ContainerState::Stopped => "stopped",
    }
}

fn health_label(health: ContainerHealth) -> &'static str {
    match health {
        ContainerHealth::Healthy => "healthy",
        ContainerHealth::Unhealthy => "unhealthy",
        ContainerHealth::Starting => "starting",
        ContainerHealth::NotConfigured => "not configured",
    }
}

fn port_label(ports: &[PublishedPort]) -> String {
    if ports.is_empty() {
        return "3306/tcp not published".to_owned();
    }
    ports
        .iter()
        .map(|port| format!("{}:{} → 3306/tcp", port.host_ip, port.host_port))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;
    use crate::domain::{ContainerId, ContainerName};

    fn candidate(state: ContainerState) -> DockerContainerCandidate {
        DockerContainerCandidate {
            id: ContainerId::try_from("a".repeat(64)).unwrap(),
            name: ContainerName::try_from("mysql-8").unwrap(),
            image: "mysql:8".to_owned(),
            state,
            health: ContainerHealth::Healthy,
            exposes_mysql_port: true,
            published_ports: vec![PublishedPort {
                host_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                host_port: 3306,
            }],
            networks: vec!["bridge".to_owned()],
            managed_by_reprodb: false,
        }
    }

    #[test]
    fn discovery_output_shows_real_selection_information_and_bind_warning() {
        let output = render_discovered(
            &OutputStyle::plain(),
            "desktop-linux",
            &[candidate(ContainerState::Running)],
        );

        assert!(output.contains("mysql-8  mysql:8  running"));
        assert!(output.contains("0.0.0.0:3306 → 3306/tcp"));
        assert!(output.contains("Port is exposed on every host interface"));
        assert!(output.contains("health: healthy"));
    }

    #[test]
    fn stopped_container_remains_visible_in_the_selection() {
        let labels = choice_labels(&[candidate(ContainerState::Stopped)]);

        assert!(labels[0].contains("mysql-8"));
        assert!(labels[0].contains("stopped"));
    }

    #[test]
    fn configured_output_explains_allowlist_and_target_trust() {
        let configured = LocalTargetConfigured {
            container_name: ContainerName::try_from("mysql-8").unwrap(),
            server_version: "8.4.4".parse().unwrap(),
            vendor: "MySQL Community Server".to_owned(),
            docker_context: "desktop-linux".to_owned(),
            tenant_database_prefix: "salt_".to_owned(),
            managed_by_reprodb: false,
            replaced_existing: false,
            previous_credential_was_missing: false,
        };

        let output = render_configured(&OutputStyle::plain(), &configured);

        assert!(output.contains("Tenant database allowlist: salt_*"));
        assert!(output.contains("existing container confirmed during setup"));
    }
}
