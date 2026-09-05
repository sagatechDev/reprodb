use async_trait::async_trait;

use crate::{
    application::{
        LocalTargetVerifier, NewLocalTargetInput, TargetVerificationError, VerifiedLocalTarget,
    },
    domain::MysqlTlsMode,
    infrastructure::{
        mysql::{ClientCatalog, DockerClientError, DockerMysqlClientRuntime},
        process::ProcessRunner,
    },
};

const BOOTSTRAP_MYSQL_SERIES: &str = "8.4";

pub struct DockerLocalTargetVerifier<R> {
    runtime: DockerMysqlClientRuntime<R>,
}

impl<R> DockerLocalTargetVerifier<R>
where
    R: ProcessRunner,
{
    pub fn new(runner: R) -> Self {
        Self {
            runtime: DockerMysqlClientRuntime::new(runner),
        }
    }
}

#[async_trait]
impl<R> LocalTargetVerifier for DockerLocalTargetVerifier<R>
where
    R: ProcessRunner,
{
    async fn verify(
        &self,
        input: &NewLocalTargetInput,
    ) -> Result<VerifiedLocalTarget, TargetVerificationError> {
        let client = ClientCatalog::resolve(BOOTSTRAP_MYSQL_SERIES)
            .map_err(|_| TargetVerificationError::ClientUnavailable)?;
        let prepared = self
            .runtime
            .prepare(&input.docker_context, client.series(), client.image())
            .await
            .map_err(map_runtime_error)?;
        let option_file = self
            .runtime
            .create_container_option_file(
                3306,
                &input.username,
                &input.password,
                MysqlTlsMode::Required,
            )
            .map_err(map_runtime_error)?;
        let server = self
            .runtime
            .probe_container_connection(&prepared, &input.container_id, &option_file)
            .await
            .map_err(map_runtime_error)?;

        Ok(VerifiedLocalTarget {
            server_version: server.version,
            vendor: server.vendor,
            client,
        })
    }
}

fn map_runtime_error(error: DockerClientError) -> TargetVerificationError {
    match error {
        DockerClientError::AuthenticationFailed => TargetVerificationError::AuthenticationFailed,
        DockerClientError::SourceNetworkUnavailable => {
            TargetVerificationError::ConnectionUnavailable
        }
        DockerClientError::Catalog(_)
        | DockerClientError::ImagePullFailed
        | DockerClientError::ImageInspectFailed
        | DockerClientError::InvalidImageMetadata
        | DockerClientError::ImageDigestMismatch
        | DockerClientError::VersionProbeFailed
        | DockerClientError::IncompatibleClientVersion => {
            TargetVerificationError::ClientUnavailable
        }
        DockerClientError::Process(_)
        | DockerClientError::DockerUnavailable
        | DockerClientError::InvalidDockerContext
        | DockerClientError::ConnectionProbeFailed => {
            TargetVerificationError::ConnectionUnavailable
        }
        DockerClientError::OptionFile(_)
        | DockerClientError::OptionFilePathNotAbsolute
        | DockerClientError::InvalidServerMetadata => TargetVerificationError::InvalidMetadata,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_target_authentication_without_external_stderr() {
        assert_eq!(
            map_runtime_error(DockerClientError::AuthenticationFailed),
            TargetVerificationError::AuthenticationFailed
        );
        assert_eq!(
            map_runtime_error(DockerClientError::ImageDigestMismatch),
            TargetVerificationError::ClientUnavailable
        );
    }
}
