use std::{collections::BTreeMap, net::IpAddr};

use serde::Deserialize;
use thiserror::Error;

use crate::{
    domain::{ContainerId, ContainerName},
    infrastructure::process::{ProcessError, ProcessRunner, ProcessSpec},
};

const REPRODB_TARGET_LABEL: &str = "com.sagatech.reprodb.target";
const MYSQL_PORT: &str = "3306/tcp";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContainerState {
    Running,
    Stopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContainerHealth {
    Healthy,
    Unhealthy,
    Starting,
    NotConfigured,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishedPort {
    pub host_ip: IpAddr,
    pub host_port: u16,
}

impl PublishedPort {
    pub const fn is_exposed_on_all_interfaces(&self) -> bool {
        self.host_ip.is_unspecified()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DockerContainerCandidate {
    pub id: ContainerId,
    pub name: ContainerName,
    pub image: String,
    pub state: ContainerState,
    pub health: ContainerHealth,
    pub exposes_mysql_port: bool,
    pub published_ports: Vec<PublishedPort>,
    pub networks: Vec<String>,
    pub managed_by_reprodb: bool,
}

#[derive(Debug, Error)]
pub enum DockerDiscoveryError {
    #[error(transparent)]
    Process(#[from] ProcessError),

    #[error("Docker or its current context is unavailable")]
    DockerUnavailable,

    #[error("the current Docker context is not a local Unix-socket context")]
    RemoteContext,

    #[error("Docker returned invalid container metadata")]
    InvalidMetadata,

    #[error("the selected MySQL container could not be started")]
    ContainerStartFailed,
}

pub struct DockerTargetDiscovery<R> {
    runner: R,
}

impl<R> DockerTargetDiscovery<R>
where
    R: ProcessRunner,
{
    pub fn new(runner: R) -> Self {
        Self { runner }
    }

    pub async fn discover(
        &self,
    ) -> Result<(String, Vec<DockerContainerCandidate>), DockerDiscoveryError> {
        let context = self.current_local_context().await?;
        let ids = self.list_container_ids(&context).await?;
        if ids.is_empty() {
            return Ok((context, Vec::new()));
        }

        let output = self
            .runner
            .output(
                &docker_spec(&context)
                    .args(["container", "inspect"])
                    .args(ids.iter().map(ContainerId::as_str)),
            )
            .await?;
        if !output.success {
            return Err(DockerDiscoveryError::DockerUnavailable);
        }

        let inspected: Vec<InspectedContainer> = serde_json::from_slice(&output.stdout)
            .map_err(|_| DockerDiscoveryError::InvalidMetadata)?;
        let mut candidates = inspected
            .into_iter()
            .filter(InspectedContainer::looks_like_mysql)
            .map(DockerContainerCandidate::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        candidates.sort_by(|left, right| left.name.cmp(&right.name));

        Ok((context, candidates))
    }

    pub async fn start_container(
        &self,
        context: &str,
        container: &ContainerId,
    ) -> Result<(), DockerDiscoveryError> {
        validate_context(context)?;
        let output = self
            .runner
            .output(&docker_spec(context).args(["container", "start", container.as_str()]))
            .await?;
        if !output.success {
            return Err(DockerDiscoveryError::ContainerStartFailed);
        }
        Ok(())
    }

    async fn current_local_context(&self) -> Result<String, DockerDiscoveryError> {
        let output = self
            .runner
            .output(&ProcessSpec::new("docker").args(["context", "show"]))
            .await?;
        if !output.success {
            return Err(DockerDiscoveryError::DockerUnavailable);
        }
        let context = parse_safe_text(&output.stdout, 128)?;

        let output = self
            .runner
            .output(&ProcessSpec::new("docker").args([
                "context",
                "inspect",
                "--format",
                "{{json .Endpoints.docker.Host}}",
                &context,
            ]))
            .await?;
        if !output.success {
            return Err(DockerDiscoveryError::DockerUnavailable);
        }
        let endpoint: String = serde_json::from_slice(output.stdout.trim_ascii())
            .map_err(|_| DockerDiscoveryError::InvalidMetadata)?;
        if !endpoint.starts_with("unix://") {
            return Err(DockerDiscoveryError::RemoteContext);
        }

        Ok(context)
    }

    async fn list_container_ids(
        &self,
        context: &str,
    ) -> Result<Vec<ContainerId>, DockerDiscoveryError> {
        let output = self
            .runner
            .output(&docker_spec(context).args([
                "container",
                "ls",
                "--all",
                "--no-trunc",
                "--quiet",
            ]))
            .await?;
        if !output.success {
            return Err(DockerDiscoveryError::DockerUnavailable);
        }

        std::str::from_utf8(&output.stdout)
            .map_err(|_| DockerDiscoveryError::InvalidMetadata)?
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| {
                ContainerId::try_from(line).map_err(|_| DockerDiscoveryError::InvalidMetadata)
            })
            .collect()
    }
}

impl InspectedContainer {
    fn looks_like_mysql(&self) -> bool {
        self.config
            .labels
            .as_ref()
            .and_then(|labels| labels.get(REPRODB_TARGET_LABEL))
            .is_some_and(|value| value == "true")
            || image_name(&self.config.image).is_some_and(|name| {
                name.eq_ignore_ascii_case("mysql")
                    || name
                        .get(..6)
                        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("mysql-"))
            })
            || self.config.exposed_ports.contains_key(MYSQL_PORT)
            || self.network_settings.ports.contains_key(MYSQL_PORT)
    }
}

impl TryFrom<InspectedContainer> for DockerContainerCandidate {
    type Error = DockerDiscoveryError;

    fn try_from(mut container: InspectedContainer) -> Result<Self, Self::Error> {
        let id = ContainerId::try_from(container.id)
            .map_err(|_| DockerDiscoveryError::InvalidMetadata)?;
        let name = ContainerName::try_from(container.name.trim_start_matches('/'))
            .map_err(|_| DockerDiscoveryError::InvalidMetadata)?;
        validate_metadata_text(&container.config.image, 512)?;

        let state = if container.state.status == "running" {
            ContainerState::Running
        } else {
            ContainerState::Stopped
        };
        let health = match container
            .state
            .health
            .as_ref()
            .map(|health| health.status.as_str())
        {
            Some("healthy") => ContainerHealth::Healthy,
            Some("unhealthy") => ContainerHealth::Unhealthy,
            Some("starting") => ContainerHealth::Starting,
            Some(_) => return Err(DockerDiscoveryError::InvalidMetadata),
            None => ContainerHealth::NotConfigured,
        };
        let published_ports = container
            .network_settings
            .ports
            .remove(MYSQL_PORT)
            .flatten()
            .unwrap_or_default()
            .into_iter()
            .map(PublishedPort::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let networks = container
            .network_settings
            .networks
            .into_keys()
            .map(|network| {
                validate_metadata_text(&network, 255)?;
                Ok(network)
            })
            .collect::<Result<Vec<_>, DockerDiscoveryError>>()?;
        let managed_by_reprodb = container
            .config
            .labels
            .as_ref()
            .and_then(|labels| labels.get(REPRODB_TARGET_LABEL))
            .is_some_and(|value| value == "true");
        let exposes_mysql = container.config.exposed_ports.contains_key(MYSQL_PORT);

        let candidate = Self {
            id,
            name,
            image: container.config.image,
            state,
            health,
            exposes_mysql_port: exposes_mysql,
            published_ports,
            networks,
            managed_by_reprodb,
        };
        Ok(candidate)
    }
}

impl TryFrom<PortBinding> for PublishedPort {
    type Error = DockerDiscoveryError;

    fn try_from(binding: PortBinding) -> Result<Self, Self::Error> {
        let host_ip = binding
            .host_ip
            .parse()
            .map_err(|_| DockerDiscoveryError::InvalidMetadata)?;
        let host_port = binding
            .host_port
            .parse()
            .map_err(|_| DockerDiscoveryError::InvalidMetadata)?;
        if host_port == 0 {
            return Err(DockerDiscoveryError::InvalidMetadata);
        }
        Ok(Self { host_ip, host_port })
    }
}

fn docker_spec(context: &str) -> ProcessSpec {
    ProcessSpec::new("docker").args(["--context", context])
}

fn parse_safe_text(bytes: &[u8], max: usize) -> Result<String, DockerDiscoveryError> {
    let value = std::str::from_utf8(bytes)
        .map_err(|_| DockerDiscoveryError::InvalidMetadata)?
        .trim();
    validate_metadata_text(value, max)?;
    validate_context(value)?;
    Ok(value.to_owned())
}

fn validate_context(value: &str) -> Result<(), DockerDiscoveryError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
    {
        return Err(DockerDiscoveryError::InvalidMetadata);
    }
    Ok(())
}

fn validate_metadata_text(value: &str, max: usize) -> Result<(), DockerDiscoveryError> {
    if value.is_empty() || value.chars().count() > max || value.chars().any(char::is_control) {
        return Err(DockerDiscoveryError::InvalidMetadata);
    }
    Ok(())
}

fn image_name(image: &str) -> Option<&str> {
    image.rsplit('/').next()?.split([':', '@']).next()
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InspectedContainer {
    id: String,
    name: String,
    config: InspectedConfig,
    state: InspectedState,
    network_settings: InspectedNetworkSettings,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InspectedConfig {
    image: String,
    #[serde(default)]
    labels: Option<BTreeMap<String, String>>,
    #[serde(default)]
    exposed_ports: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InspectedState {
    status: String,
    #[serde(default)]
    health: Option<InspectedHealth>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InspectedHealth {
    status: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InspectedNetworkSettings {
    #[serde(default)]
    ports: BTreeMap<String, Option<Vec<PortBinding>>>,
    #[serde(default)]
    networks: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PortBinding {
    host_ip: String,
    host_port: String,
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use async_trait::async_trait;

    use super::*;
    use crate::infrastructure::process::{ProcessOutput, ProcessRunner};

    struct FakeRunner {
        outputs: Mutex<VecDeque<ProcessOutput>>,
        commands: Mutex<Vec<ProcessSpec>>,
    }

    impl FakeRunner {
        fn new(outputs: impl IntoIterator<Item = ProcessOutput>) -> Self {
            Self {
                outputs: Mutex::new(outputs.into_iter().collect()),
                commands: Mutex::new(Vec::new()),
            }
        }

        fn commands(&self) -> Vec<ProcessSpec> {
            self.commands.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ProcessRunner for FakeRunner {
        async fn output(&self, spec: &ProcessSpec) -> Result<ProcessOutput, ProcessError> {
            self.commands.lock().unwrap().push(spec.clone());
            Ok(self.outputs.lock().unwrap().pop_front().unwrap())
        }
    }

    fn id(character: char) -> String {
        character.to_string().repeat(64)
    }

    fn inspect_json() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!([
            {
                "Id": id('a'),
                "Name": "/mysql-8",
                "Config": {
                    "Image": "mysql:8",
                    "Labels": {},
                    "ExposedPorts": {"3306/tcp": {}}
                },
                "State": {"Status": "running", "Health": {"Status": "healthy"}},
                "NetworkSettings": {
                    "Ports": {"3306/tcp": [{"HostIp": "0.0.0.0", "HostPort": "3306"}]},
                    "Networks": {"bridge": {}}
                }
            },
            {
                "Id": id('b'),
                "Name": "/salt-redis",
                "Config": {
                    "Image": "redis:7-alpine",
                    "Labels": {},
                    "ExposedPorts": {"6379/tcp": {}}
                },
                "State": {"Status": "running", "Health": {"Status": "future-status"}},
                "NetworkSettings": {
                    "Ports": {"6379/tcp": [{"HostIp": "127.0.0.1", "HostPort": "6379"}]},
                    "Networks": {"bridge": {}}
                }
            }
        ]))
        .unwrap()
    }

    #[tokio::test]
    async fn discovers_mysql_candidates_with_structured_docker_commands() {
        let runner = FakeRunner::new([
            ProcessOutput::success("desktop-linux\n"),
            ProcessOutput::success("\"unix:///Users/dev/.docker/run/docker.sock\"\n"),
            ProcessOutput::success(format!("{}\n{}\n", id('a'), id('b'))),
            ProcessOutput::success(inspect_json()),
        ]);
        let discovery = DockerTargetDiscovery::new(runner);

        let (context, candidates) = discovery.discover().await.unwrap();

        assert_eq!(context, "desktop-linux");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].name.as_str(), "mysql-8");
        assert_eq!(candidates[0].state, ContainerState::Running);
        assert_eq!(candidates[0].health, ContainerHealth::Healthy);
        assert_eq!(candidates[0].published_ports[0].host_port, 3306);
        assert!(candidates[0].published_ports[0].is_exposed_on_all_interfaces());

        let commands = discovery.runner.commands();
        assert_eq!(commands[0].arguments(), ["context", "show"]);
        assert_eq!(
            commands[2].arguments()[0..7],
            [
                "--context",
                "desktop-linux",
                "container",
                "ls",
                "--all",
                "--no-trunc",
                "--quiet",
            ]
        );
        assert_eq!(commands[3].program(), "docker");
        assert!(commands[3].arguments().contains(&id('a').into()));
    }

    #[tokio::test]
    async fn refuses_a_remote_docker_context_before_listing_containers() {
        let discovery = DockerTargetDiscovery::new(FakeRunner::new([
            ProcessOutput::success("remote\n"),
            ProcessOutput::success("\"ssh://developer@example.test\"\n"),
        ]));

        assert!(matches!(
            discovery.discover().await,
            Err(DockerDiscoveryError::RemoteContext)
        ));
        assert_eq!(discovery.runner.commands().len(), 2);
    }

    #[tokio::test]
    async fn accepts_a_custom_image_that_exposes_the_mysql_port() {
        let inspected = serde_json::to_vec(&serde_json::json!([{
            "Id": id('c'),
            "Name": "/custom-database",
            "Config": {
                "Image": "sagatech/database:local",
                "Labels": {},
                "ExposedPorts": {"3306/tcp": {}}
            },
            "State": {"Status": "exited"},
            "NetworkSettings": {"Ports": {"3306/tcp": null}, "Networks": {}}
        }]))
        .unwrap();
        let discovery = DockerTargetDiscovery::new(FakeRunner::new([
            ProcessOutput::success("default\n"),
            ProcessOutput::success("\"unix:///var/run/docker.sock\"\n"),
            ProcessOutput::success(format!("{}\n", id('c'))),
            ProcessOutput::success(inspected),
        ]));

        let (_, candidates) = discovery.discover().await.unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].name.as_str(), "custom-database");
        assert_eq!(candidates[0].state, ContainerState::Stopped);
        assert!(candidates[0].published_ports.is_empty());
    }

    #[tokio::test]
    async fn starts_a_selected_container_by_context_and_full_id() {
        let discovery = DockerTargetDiscovery::new(FakeRunner::new([ProcessOutput::success(
            format!("{}\n", id('a')),
        )]));
        let container = ContainerId::try_from(id('a')).unwrap();

        discovery
            .start_container("desktop-linux", &container)
            .await
            .unwrap();

        let commands = discovery.runner.commands();
        assert_eq!(
            commands[0].arguments(),
            [
                "--context",
                "desktop-linux",
                "container",
                "start",
                container.as_str(),
            ]
        );
    }
}
