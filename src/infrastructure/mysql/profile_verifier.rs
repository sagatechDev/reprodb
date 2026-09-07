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

const DISCOVERY_MYSQL_SERIES: &str = "8.4";

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
        let discovery_client = ClientCatalog::resolve(DISCOVERY_MYSQL_SERIES)
            .map_err(|_| SourceVerificationError::ClientUnavailable)?;
        let docker_context = self
            .runtime
            .current_context()
            .await
            .map_err(map_runtime_error)?;
        let prepared = self
            .prepare_client(&docker_context, discovery_client)
            .await?;
        let option_file = self
            .runtime
            .create_option_file_with_tls_material(
                &input.host,
                input.port,
                &input.username,
                &input.password,
                input.tls_mode,
                &input.tls_material,
            )
            .map_err(map_runtime_error)?;
        let discovered_server = self
            .runtime
            .probe_connection(&prepared, &option_file)
            .await
            .map_err(map_runtime_error)?;
        let detected_series = format!(
            "{}.{}",
            discovered_server.version.major, discovered_server.version.minor
        );
        let client = ClientCatalog::resolve(&detected_series).map_err(|_| {
            SourceVerificationError::UnsupportedServerSeries {
                major: discovered_server.version.major,
                minor: discovered_server.version.minor,
                supported: ClientCatalog::supported_series(),
            }
        })?;
        let server = if client == discovery_client {
            discovered_server
        } else {
            let prepared = self.prepare_client(&docker_context, client).await?;
            self.runtime
                .probe_connection(&prepared, &option_file)
                .await
                .map_err(map_runtime_error)?
        };

        Ok(VerifiedSource {
            docker_context,
            server_version: server.version,
            vendor: server.vendor,
            tls_cipher: server.tls_cipher,
            client,
        })
    }
}

impl<R> DockerSourceProfileVerifier<R>
where
    R: ProcessRunner,
{
    async fn prepare_client(
        &self,
        docker_context: &str,
        client: crate::infrastructure::mysql::ApprovedMysqlClient,
    ) -> Result<crate::infrastructure::mysql::PreparedMysqlClient, SourceVerificationError> {
        let prepared = if self.pull_missing_client {
            self.runtime
                .prepare(docker_context, client.series(), client.image())
                .await
        } else {
            self.runtime
                .prepare_existing(docker_context, client.series(), client.image())
                .await
        };
        prepared.map_err(map_runtime_error)
    }
}

fn map_runtime_error(error: DockerClientError) -> SourceVerificationError {
    match error {
        DockerClientError::DockerUnavailable | DockerClientError::InvalidDockerContext => {
            SourceVerificationError::DockerUnavailable
        }
        DockerClientError::SourceNetworkUnavailable => SourceVerificationError::NetworkUnavailable,
        DockerClientError::AuthenticationFailed => SourceVerificationError::AuthenticationFailed,
        DockerClientError::TlsValidationFailed => {
            SourceVerificationError::TlsRequiredButNotNegotiated
        }
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
    use std::{collections::VecDeque, sync::Mutex};

    use secrecy::SecretString;

    use super::*;
    use crate::{
        application::NewProfileInput,
        domain::{MysqlTlsMaterialPaths, MysqlTlsMode, ProfileName},
        infrastructure::process::{ProcessError, ProcessOutput, ProcessSpec},
    };

    struct FakeRunner {
        outputs: Mutex<VecDeque<ProcessOutput>>,
    }

    #[async_trait]
    impl ProcessRunner for FakeRunner {
        async fn output(&self, _spec: &ProcessSpec) -> Result<ProcessOutput, ProcessError> {
            Ok(self.outputs.lock().unwrap().pop_front().unwrap())
        }
    }

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

    #[tokio::test]
    async fn discovers_the_server_then_downloads_and_verifies_its_approved_client() {
        let discovery = ClientCatalog::resolve("8.4").unwrap();
        let selected = ClientCatalog::resolve("8.0").unwrap();
        let runner = FakeRunner {
            outputs: Mutex::new(VecDeque::from([
                ProcessOutput::success("desktop-linux\n"),
                ProcessOutput::success(
                    serde_json::to_vec(&vec![discovery.repository_digest()]).unwrap(),
                ),
                ProcessOutput::success("mysql  Ver 8.4.4 for Linux on aarch64\n"),
                ProcessOutput::success(
                    "8.0.40\tMySQL Community Server - GPL\t11111111-1111-4111-8111-111111111111\nSsl_cipher\tTLS_AES_256_GCM_SHA384\n",
                ),
                ProcessOutput::success(
                    serde_json::to_vec(&vec![selected.repository_digest()]).unwrap(),
                ),
                ProcessOutput::success("mysql  Ver 8.0.46 for Linux on aarch64\n"),
                ProcessOutput::success(
                    "8.0.40\tMySQL Community Server - GPL\t11111111-1111-4111-8111-111111111111\nSsl_cipher\tTLS_AES_256_GCM_SHA384\n",
                ),
            ])),
        };
        let verifier = DockerSourceProfileVerifier::new(runner);
        let input = NewProfileInput {
            name: ProfileName::try_from("sandbox").unwrap(),
            host: "sandbox.example.invalid".to_owned(),
            port: 3306,
            username: "reader".to_owned(),
            password: SecretString::from("secret"),
            tls_mode: MysqlTlsMode::Preferred,
            tls_material: MysqlTlsMaterialPaths::default(),
            production: false,
        };

        let verified = verifier.verify(&input).await.unwrap();

        assert_eq!(verified.server_version.to_string(), "8.0.40");
        assert_eq!(verified.client, selected);
    }
}
