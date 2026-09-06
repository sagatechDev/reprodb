use async_trait::async_trait;

use crate::{
    application::{
        NewProfileInput, SourceProfileVerifier, SourceVerificationError, VerifiedSource,
    },
    infrastructure::{
        mysql::{ClientCatalog, DockerClientError, DockerMysqlClientRuntime},
        process::ProcessRunner,
    },
};

const BOOTSTRAP_MYSQL_SERIES: &str = "8.4";

pub struct DockerSourceProfileVerifier<R> {
    runtime: DockerMysqlClientRuntime<R>,
    pull_missing_client: bool,
}

impl<R> DockerSourceProfileVerifier<R>
where
    R: ProcessRunner,
{
    pub fn new(runner: R) -> Self {
        Self {
            runtime: DockerMysqlClientRuntime::new(runner),
            pull_missing_client: true,
        }
    }

    pub fn read_only(runner: R) -> Self {
        Self {
            runtime: DockerMysqlClientRuntime::new(runner),
            pull_missing_client: false,
        }
    }
}

#[async_trait]
impl<R> SourceProfileVerifier for DockerSourceProfileVerifier<R>
where
    R: ProcessRunner,
{
    async fn verify(
        &self,
        input: &NewProfileInput,
    ) -> Result<VerifiedSource, SourceVerificationError> {
        let client = ClientCatalog::resolve(BOOTSTRAP_MYSQL_SERIES)
            .map_err(|_| SourceVerificationError::ClientUnavailable)?;
        let docker_context = self
            .runtime
            .current_context()
            .await
            .map_err(map_runtime_error)?;
        let prepared = if self.pull_missing_client {
            self.runtime
                .prepare(&docker_context, client.series(), client.image())
                .await
        } else {
            self.runtime
                .prepare_existing(&docker_context, client.series(), client.image())
                .await
        }
        .map_err(map_runtime_error)?;
        let option_file = self
            .runtime
            .create_option_file(
                &input.host,
                input.port,
                &input.username,
                &input.password,
                input.tls_mode,
            )
            .map_err(map_runtime_error)?;
        let server = self
            .runtime
            .probe_connection(&prepared, &option_file)
            .await
            .map_err(map_runtime_error)?;

        Ok(VerifiedSource {
            docker_context,
            server_version: server.version,
            vendor: server.vendor,
            tls_cipher: server.tls_cipher,
            client,
        })
    }
}

fn map_runtime_error(error: DockerClientError) -> SourceVerificationError {
    match error {
        DockerClientError::DockerUnavailable | DockerClientError::InvalidDockerContext => {
            SourceVerificationError::DockerUnavailable
        }
        DockerClientError::SourceNetworkUnavailable => SourceVerificationError::NetworkUnavailable,
        DockerClientError::AuthenticationFailed => SourceVerificationError::AuthenticationFailed,
        DockerClientError::InvalidServerMetadata => SourceVerificationError::InvalidMetadata,
        DockerClientError::Catalog(_)
        | DockerClientError::ImageUnavailable
        | DockerClientError::ImagePullFailed
        | DockerClientError::ImageInspectFailed
        | DockerClientError::InvalidImageMetadata
        | DockerClientError::ImageDigestMismatch
        | DockerClientError::VersionProbeFailed
        | DockerClientError::IncompatibleClientVersion => {
            SourceVerificationError::ClientUnavailable
        }
        DockerClientError::Process(_)
        | DockerClientError::OptionFile(_)
        | DockerClientError::OptionFilePathNotAbsolute
        | DockerClientError::ConnectionProbeFailed
        | DockerClientError::QueryFailed => SourceVerificationError::InvalidMetadata,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_external_errors_to_safe_application_categories() {
        assert_eq!(
            map_runtime_error(DockerClientError::AuthenticationFailed),
            SourceVerificationError::AuthenticationFailed
        );
        assert_eq!(
            map_runtime_error(DockerClientError::SourceNetworkUnavailable),
            SourceVerificationError::NetworkUnavailable
        );
        assert_eq!(
            map_runtime_error(DockerClientError::ImageDigestMismatch),
            SourceVerificationError::ClientUnavailable
        );
    }
}
