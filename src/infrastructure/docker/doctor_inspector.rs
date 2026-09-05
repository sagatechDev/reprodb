use async_trait::async_trait;

use crate::{
    application::{
        DoctorDockerContainer, DoctorDockerError, DoctorDockerInspector, DoctorDockerInventory,
    },
    infrastructure::{
        docker::{ContainerState, DockerDiscoveryError, DockerTargetDiscovery},
        process::ProcessRunner,
    },
};

pub struct DockerCliDoctorInspector<R> {
    discovery: DockerTargetDiscovery<R>,
}

impl<R> DockerCliDoctorInspector<R>
where
    R: ProcessRunner,
{
    pub fn new(runner: R) -> Self {
        Self {
            discovery: DockerTargetDiscovery::new(runner),
        }
    }
}

#[async_trait]
impl<R> DoctorDockerInspector for DockerCliDoctorInspector<R>
where
    R: ProcessRunner,
{
    async fn inspect(&self) -> Result<DoctorDockerInventory, DoctorDockerError> {
        let (context, candidates) = self
            .discovery
            .discover()
            .await
            .map_err(map_discovery_error)?;

        Ok(DoctorDockerInventory {
            context,
            containers: candidates
                .into_iter()
                .map(|candidate| DoctorDockerContainer {
                    id: candidate.id,
                    name: candidate.name,
                    running: candidate.state == ContainerState::Running,
                })
                .collect(),
        })
    }
}

fn map_discovery_error(error: DockerDiscoveryError) -> DoctorDockerError {
    match error {
        DockerDiscoveryError::RemoteContext => DoctorDockerError::RemoteContext,
        DockerDiscoveryError::InvalidMetadata => DoctorDockerError::InvalidMetadata,
        DockerDiscoveryError::Process(_)
        | DockerDiscoveryError::DockerUnavailable
        | DockerDiscoveryError::ContainerStartFailed => DoctorDockerError::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_discovery_failures_to_safe_doctor_categories() {
        assert_eq!(
            map_discovery_error(DockerDiscoveryError::RemoteContext),
            DoctorDockerError::RemoteContext
        );
        assert_eq!(
            map_discovery_error(DockerDiscoveryError::InvalidMetadata),
            DoctorDockerError::InvalidMetadata
        );
    }
}
