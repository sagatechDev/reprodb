use async_trait::async_trait;

use crate::{
    application::{
        LocalTargetAttestation, LocalTargetAttestationError, LocalTargetAttestationRequest,
        LocalTargetAttestor, LocalTargetVerifier, NewLocalTargetInput, TargetVerificationError,
    },
    infrastructure::{
        config::LocalTargetTrust,
        docker::{ContainerState, DockerDiscoveryError, DockerTargetDiscovery},
        mysql::DockerLocalTargetVerifier,
        process::ProcessRunner,
    },
};

#[derive(Clone, Debug)]
pub struct DockerLocalTargetAttestor<R> {
    runner: R,
}

impl<R> DockerLocalTargetAttestor<R> {
    pub fn new(runner: R) -> Self {
        Self { runner }
    }
}

#[async_trait]
impl<R> LocalTargetAttestor for DockerLocalTargetAttestor<R>
where
    R: ProcessRunner + Clone,
{
    async fn attest(
        &self,
        request: LocalTargetAttestationRequest<'_>,
    ) -> Result<LocalTargetAttestation, LocalTargetAttestationError> {
        let (active_context, candidates) = DockerTargetDiscovery::new(self.runner.clone())
            .discover()
            .await
            .map_err(map_discovery_error)?;
        if active_context != request.configured.docker_context {
            return Err(LocalTargetAttestationError::ContextChanged);
        }

        let candidate = candidates
            .into_iter()
            .find(|candidate| {
                candidate.id == request.configured.container_id
                    && candidate.name == request.configured.container_name
            })
            .ok_or(LocalTargetAttestationError::ContainerNotFound)?;
        if candidate.state != ContainerState::Running {
            return Err(LocalTargetAttestationError::ContainerNotRunning);
        }

        let verified = DockerLocalTargetVerifier::read_only(self.runner.clone())
            .verify(&NewLocalTargetInput {
                docker_context: active_context.clone(),
                container_name: candidate.name.clone(),
                container_id: candidate.id.clone(),
                username: request.configured.username.clone(),
                password: request.password.clone(),
                central_database: request.configured.central_database.clone(),
                managed_by_reprodb: matches!(
                    request.configured.trust,
                    LocalTargetTrust::ReprodbManaged
                ),
            })
            .await
            .map_err(map_verification_error)?;

        Ok(LocalTargetAttestation {
            docker_context: active_context,
            container_name: candidate.name,
            container_id: candidate.id,
            managed_by_reprodb: candidate.managed_by_reprodb,
            server_version: verified.server_version,
            server_uuid: verified.server_uuid,
            vendor: verified.vendor,
            client: verified.client,
        })
    }
}

fn map_discovery_error(error: DockerDiscoveryError) -> LocalTargetAttestationError {
    match error {
        DockerDiscoveryError::RemoteContext => LocalTargetAttestationError::RemoteContext,
        DockerDiscoveryError::InvalidMetadata => LocalTargetAttestationError::InvalidMetadata,
        DockerDiscoveryError::Process(_)
        | DockerDiscoveryError::DockerUnavailable
        | DockerDiscoveryError::ContainerStartFailed => {
            LocalTargetAttestationError::DockerUnavailable
        }
    }
}

fn map_verification_error(error: TargetVerificationError) -> LocalTargetAttestationError {
    match error {
        TargetVerificationError::ContainerNotRunning => {
            LocalTargetAttestationError::ContainerNotRunning
        }
        TargetVerificationError::ClientUnavailable
        | TargetVerificationError::UnsupportedServerSeries { .. } => {
            LocalTargetAttestationError::ClientUnavailable
        }
        TargetVerificationError::ConnectionUnavailable => {
            LocalTargetAttestationError::ConnectionUnavailable
        }
        TargetVerificationError::AuthenticationFailed => {
            LocalTargetAttestationError::AuthenticationFailed
        }
        TargetVerificationError::InvalidMetadata => LocalTargetAttestationError::InvalidMetadata,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_failures_are_mapped_without_diagnostics() {
        assert_eq!(
            map_discovery_error(DockerDiscoveryError::RemoteContext),
            LocalTargetAttestationError::RemoteContext
        );
        assert_eq!(
            map_verification_error(TargetVerificationError::AuthenticationFailed),
            LocalTargetAttestationError::AuthenticationFailed
        );
    }
}
