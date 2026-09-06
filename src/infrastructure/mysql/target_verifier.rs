use async_trait::async_trait;
use std::time::Duration;

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
    pull_missing_client: bool,
    probe_attempts: usize,
    retry_delay: Duration,
}

impl<R> DockerLocalTargetVerifier<R>
where
    R: ProcessRunner,
{
    pub fn new(runner: R) -> Self {
        Self {
            runtime: DockerMysqlClientRuntime::new(runner),
            pull_missing_client: true,
            probe_attempts: 20,
            retry_delay: Duration::from_millis(500),
        }
    }

    pub fn read_only(runner: R) -> Self {
        Self {
            runtime: DockerMysqlClientRuntime::new(runner),
            pull_missing_client: false,
            probe_attempts: 1,
            retry_delay: Duration::ZERO,
        }
    }

    #[cfg(test)]
    fn with_retry_policy(runner: R, probe_attempts: usize, retry_delay: Duration) -> Self {
        Self {
            runtime: DockerMysqlClientRuntime::new(runner),
            pull_missing_client: true,
            probe_attempts,
            retry_delay,
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
        let prepared = if self.pull_missing_client {
            self.runtime
                .prepare(&input.docker_context, client.series(), client.image())
                .await
        } else {
            self.runtime
                .prepare_existing(&input.docker_context, client.series(), client.image())
                .await
        }
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
            .probe_until_ready(&prepared, &input.container_id, &option_file)
            .await?;

        Ok(VerifiedLocalTarget {
            server_version: server.version,
            vendor: server.vendor,
            tls_cipher: server.tls_cipher,
            client,
        })
    }
}

impl<R> DockerLocalTargetVerifier<R>
where
    R: ProcessRunner,
{
    async fn probe_until_ready(
        &self,
        client: &crate::infrastructure::mysql::PreparedMysqlClient,
        container: &crate::domain::ContainerId,
        option_file: &crate::infrastructure::credentials::MysqlOptionFile,
    ) -> Result<crate::infrastructure::mysql::MysqlServerInfo, TargetVerificationError> {
        for attempt in 1..=self.probe_attempts {
            match self
                .runtime
                .probe_container_connection(client, container, option_file)
                .await
            {
                Ok(server) => return Ok(server),
                Err(error)
                    if attempt < self.probe_attempts && is_transient_connection_error(&error) =>
                {
                    tokio::time::sleep(self.retry_delay).await;
                }
                Err(error) => return Err(map_runtime_error(error)),
            }
        }

        Err(TargetVerificationError::ConnectionUnavailable)
    }
}

fn is_transient_connection_error(error: &DockerClientError) -> bool {
    matches!(
        error,
        DockerClientError::SourceNetworkUnavailable | DockerClientError::ConnectionProbeFailed
    )
}

fn map_runtime_error(error: DockerClientError) -> TargetVerificationError {
    match error {
        DockerClientError::AuthenticationFailed => TargetVerificationError::AuthenticationFailed,
        DockerClientError::SourceNetworkUnavailable => {
            TargetVerificationError::ConnectionUnavailable
        }
        DockerClientError::Catalog(_)
        | DockerClientError::ImageUnavailable
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
        | DockerClientError::ConnectionProbeFailed
        | DockerClientError::QueryFailed => TargetVerificationError::ConnectionUnavailable,
        DockerClientError::OptionFile(_)
        | DockerClientError::OptionFilePathNotAbsolute
        | DockerClientError::InvalidServerMetadata => TargetVerificationError::InvalidMetadata,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use secrecy::SecretString;

    use super::*;
    use crate::{
        application::NewLocalTargetInput,
        domain::{ContainerId, ContainerName, DatabaseName},
        infrastructure::process::{ProcessError, ProcessOutput, ProcessSpec},
    };

    struct FakeRunner {
        outputs: Mutex<VecDeque<ProcessOutput>>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ProcessRunner for FakeRunner {
        async fn output(&self, _spec: &ProcessSpec) -> Result<ProcessOutput, ProcessError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.outputs.lock().unwrap().pop_front().unwrap())
        }
    }

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

    #[tokio::test]
    async fn retries_a_transient_connection_while_mysql_starts() {
        let client = ClientCatalog::resolve("8.4").unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let runner = FakeRunner {
            outputs: Mutex::new(VecDeque::from([
                ProcessOutput::success(
                    serde_json::to_vec(&vec![client.repository_digest()]).unwrap(),
                ),
                ProcessOutput::success("mysql  Ver 8.4.4 for Linux on aarch64\n"),
                ProcessOutput::failure(1, "Can't connect to MySQL server"),
                ProcessOutput::success("8.4.4\tMySQL Community Server - GPL\n"),
            ])),
            calls: Arc::clone(&calls),
        };
        let verifier =
            DockerLocalTargetVerifier::with_retry_policy(runner, 2, Duration::from_millis(0));
        let input = NewLocalTargetInput {
            docker_context: "desktop-linux".to_owned(),
            container_name: ContainerName::try_from("mysql-8").unwrap(),
            container_id: ContainerId::try_from("a".repeat(64)).unwrap(),
            username: "root".to_owned(),
            password: SecretString::from("secret"),
            central_database: DatabaseName::try_from("salt_central").unwrap(),
            tenant_database_prefix: "salt_".to_owned(),
            managed_by_reprodb: false,
        };

        let verified = verifier.verify(&input).await.unwrap();

        assert_eq!(verified.server_version.to_string(), "8.4.4");
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }
}
