use std::{ffi::OsString, path::Path, sync::Arc};

use async_trait::async_trait;
use thiserror::Error;

use crate::{
    application::AuthorizedRemoteTarget,
    domain::{DumpArtifactMetadata, MysqlServerUuid},
    infrastructure::{
        cancellation::CancellationToken,
        credentials::{
            MYSQL_OPTION_FILE_CONTAINER_PATH, MYSQL_SECRETS_CONTAINER_DIRECTORY, MysqlOptionFile,
        },
        docker::ephemeral_container_name,
        mysql::{
            DockerClientError, DockerMysqlClientRuntime, ImportProgressObserver, NoImportProgress,
            RestoreExecutorError, RestoreFailureKind, RestoreMetrics,
        },
        process::{ProcessSpec, TokioProcessRunner},
        restore_artifact::ValidatedRestoreArtifact,
    },
};

use super::restore_executor::{classify_failure, run_without_stdin, stream_import};

#[async_trait]
pub trait RemoteImportExecutor: Send + Sync {
    async fn ensure_database(
        &self,
        target: &AuthorizedRemoteTarget,
        metadata: &DumpArtifactMetadata,
    ) -> Result<(), RemoteImportError>;

    async fn import(
        &self,
        target: &AuthorizedRemoteTarget,
        artifact: &ValidatedRestoreArtifact,
    ) -> Result<RestoreMetrics, RemoteImportError>;
}

#[derive(Clone)]
pub struct DockerMysqlRemoteImportExecutor {
    cancellation: CancellationToken,
    progress: Arc<dyn ImportProgressObserver>,
}

impl Default for DockerMysqlRemoteImportExecutor {
    fn default() -> Self {
        Self::new(CancellationToken::default())
    }
}

impl DockerMysqlRemoteImportExecutor {
    pub fn new(cancellation: CancellationToken) -> Self {
        Self {
            cancellation,
            progress: Arc::new(NoImportProgress),
        }
    }

    pub fn with_progress(mut self, progress: Arc<dyn ImportProgressObserver>) -> Self {
        self.progress = progress;
        self
    }
}

#[async_trait]
impl RemoteImportExecutor for DockerMysqlRemoteImportExecutor {
    async fn ensure_database(
        &self,
        target: &AuthorizedRemoteTarget,
        metadata: &DumpArtifactMetadata,
    ) -> Result<(), RemoteImportError> {
        if self.cancellation.is_cancelled() {
            return Err(RestoreExecutorError::Interrupted.into());
        }
        let option_file = remote_option_file(target)?;
        let operation_container = ephemeral_container_name("push");
        let spec = ensure_process_spec(target, metadata, option_file.path(), &operation_container);
        let (status, diagnostic) = run_without_stdin(
            &spec,
            &self.cancellation,
            target.docker_context(),
            &operation_container,
        )
        .await?;
        if !status.success() {
            return Err(RemoteImportError::EnsureFailed {
                exit_code: status.code(),
                kind: classify_remote_failure(&diagnostic.bytes),
                stderr_truncated: diagnostic.truncated,
            });
        }
        Ok(())
    }

    async fn import(
        &self,
        target: &AuthorizedRemoteTarget,
        artifact: &ValidatedRestoreArtifact,
    ) -> Result<RestoreMetrics, RemoteImportError> {
        if self.cancellation.is_cancelled() {
            return Err(RestoreExecutorError::Interrupted.into());
        }
        let option_file = remote_option_file(target)?;
        let operation_container = ephemeral_container_name("push");
        let spec = import_process_spec(target, artifact, option_file.path(), &operation_container);
        let preamble = format!("{}\n", server_identity_guard(target.server_uuid()));
        let streamed = stream_import(
            &spec,
            preamble.as_bytes(),
            artifact,
            &self.cancellation,
            target.docker_context(),
            &operation_container,
            self.progress.as_ref(),
        )
        .await?;
        if !streamed.status.success() {
            return Err(RemoteImportError::ImportFailed {
                exit_code: streamed.status.code(),
                kind: classify_remote_failure(&streamed.diagnostic.bytes),
                stderr_truncated: streamed.diagnostic.truncated,
            });
        }
        Ok(streamed.verified_metrics(artifact)?)
    }
}

/// Fails the session (error 1242 on line 1) unless it is connected to the
/// server the gate attested. Each write opens a new connection through the
/// profile host, so DNS or a reassigned endpoint must not redirect it.
fn server_identity_guard(server_uuid: &MysqlServerUuid) -> String {
    format!("DO IF(@@server_uuid = '{server_uuid}', 0, (SELECT 1 UNION ALL SELECT 2));")
}

fn remote_option_file(
    target: &AuthorizedRemoteTarget,
) -> Result<MysqlOptionFile, DockerClientError> {
    DockerMysqlClientRuntime::new(TokioProcessRunner).create_option_file_with_tls_material(
        target.host(),
        target.port(),
        target.username(),
        target.password(),
        target.tls_mode(),
        target.tls_material(),
    )
}

fn ensure_process_spec(
    target: &AuthorizedRemoteTarget,
    metadata: &DumpArtifactMetadata,
    option_file: &Path,
    operation_container: &str,
) -> ProcessSpec {
    let database = target.database().as_str();
    let sql = format!(
        "{} CREATE DATABASE IF NOT EXISTS `{database}` CHARACTER SET {} COLLATE {};",
        server_identity_guard(target.server_uuid()),
        metadata.database_charset,
        metadata.database_collation
    );
    remote_mysql_process_spec(target, option_file, false, operation_container)
        .args(["--execute", &sql])
}

fn import_process_spec(
    target: &AuthorizedRemoteTarget,
    artifact: &ValidatedRestoreArtifact,
    option_file: &Path,
    operation_container: &str,
) -> ProcessSpec {
    remote_mysql_process_spec(target, option_file, true, operation_container).args([
        OsString::from("--binary-mode"),
        OsString::from(format!("--database={}", target.database())),
        OsString::from(format!(
            "--default-character-set={}",
            artifact.metadata().database_charset
        )),
    ])
}

/// Same client container as the dump: the profile host is reached through
/// `host-gateway`, never through another container's network namespace.
fn remote_mysql_process_spec(
    target: &AuthorizedRemoteTarget,
    option_file: &Path,
    interactive: bool,
    operation_container: &str,
) -> ProcessSpec {
    let mut mount = OsString::from("type=bind,src=");
    mount.push(option_file.parent().unwrap_or(option_file).as_os_str());
    mount.push(format!(",dst={MYSQL_SECRETS_CONTAINER_DIRECTORY},readonly"));

    let mut spec = ProcessSpec::new("docker").args([
        OsString::from("--context"),
        OsString::from(target.docker_context()),
        OsString::from("run"),
        OsString::from("--rm"),
        OsString::from("--name"),
        OsString::from(operation_container),
    ]);
    if interactive {
        spec = spec.arg("-i");
    }
    spec = spec.args([
        OsString::from("--pull=never"),
        OsString::from("--add-host=host.docker.internal:host-gateway"),
        OsString::from("--mount"),
        mount,
        OsString::from(target.client().image()),
        OsString::from("mysql"),
        OsString::from(format!(
            "--defaults-file={MYSQL_OPTION_FILE_CONTAINER_PATH}"
        )),
    ]);
    if target.client().supports_no_login_paths() {
        spec = spec.arg("--no-login-paths");
    }
    spec
}

fn classify_remote_failure(stderr: &[u8]) -> RemoteFailureKind {
    let lowered = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    if lowered.contains("error 1242") && lowered.contains("at line 1:") {
        return RemoteFailureKind::DestinationChanged;
    }
    // MySQL reports a missing database grant (1044) as "Access denied …
    // to database"; that is a privilege problem, not a wrong password.
    if lowered.contains("access denied") && lowered.contains("to database") {
        return RemoteFailureKind::Permission;
    }
    classify_failure(stderr).into()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteFailureKind {
    Authentication,
    Permission,
    DestinationUnavailable,
    DestinationChanged,
    Sql,
    DockerUnavailable,
    Unknown,
}

impl From<RestoreFailureKind> for RemoteFailureKind {
    fn from(kind: RestoreFailureKind) -> Self {
        match kind {
            RestoreFailureKind::Authentication => Self::Authentication,
            RestoreFailureKind::Permission => Self::Permission,
            RestoreFailureKind::TargetUnavailable => Self::DestinationUnavailable,
            RestoreFailureKind::Sql => Self::Sql,
            RestoreFailureKind::DockerUnavailable => Self::DockerUnavailable,
            RestoreFailureKind::Unknown => Self::Unknown,
        }
    }
}

impl std::fmt::Display for RemoteFailureKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Authentication => {
                "the destination rejected the profile credential; remove and re-add the profile"
            }
            Self::Permission => {
                "the destination user lacks privileges to create or write the selected database"
            }
            Self::DestinationUnavailable => {
                "the destination MySQL server became unreachable; check the network and run `reprodb doctor`"
            }
            Self::DestinationChanged => {
                "the profile host now reaches a different MySQL server than the one validated; nothing was written in that session"
            }
            Self::Sql => "MySQL rejected a statement from the validated dump",
            Self::DockerUnavailable => {
                "Docker became unavailable; start Docker and run `reprodb doctor`"
            }
            Self::Unknown => "the MySQL client returned an unclassified push failure",
        })
    }
}

#[derive(Debug, Error)]
pub enum RemoteImportError {
    #[error(transparent)]
    Client(#[from] DockerClientError),

    #[error(
        "{0}; the remote database may be partially imported and rerunning the same push imports over it"
    )]
    Stream(#[from] RestoreExecutorError),

    #[error(
        "remote database creation failed: {kind} (exit code {exit_code:?}, diagnostics truncated: {stderr_truncated})"
    )]
    EnsureFailed {
        exit_code: Option<i32>,
        kind: RemoteFailureKind,
        stderr_truncated: bool,
    },

    #[error(
        "remote import failed: {kind} (exit code {exit_code:?}, diagnostics truncated: {stderr_truncated}); the remote database may be partially imported and rerunning the same push imports over it"
    )]
    ImportFailed {
        exit_code: Option<i32>,
        kind: RemoteFailureKind,
        stderr_truncated: bool,
    },
}

#[cfg(test)]
mod tests {
    use crate::{
        domain::DatabaseName, infrastructure::restore_artifact::test_support::validated_artifact,
    };

    use super::*;

    fn arguments(spec: &ProcessSpec) -> Vec<String> {
        spec.arguments()
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    fn target() -> AuthorizedRemoteTarget {
        AuthorizedRemoteTarget::for_test(DatabaseName::try_from("salt_sagatec_qa").unwrap())
    }

    #[test]
    fn the_remote_client_reaches_the_profile_host_and_never_joins_a_container_network() {
        let arguments = arguments(&remote_mysql_process_spec(
            &target(),
            Path::new("/tmp/reprodb/client.cnf"),
            true,
            "reprodb-push-0123456789abcdef0123456789abcdef",
        ));

        assert!(arguments.contains(&"-i".to_owned()));
        assert!(!arguments.contains(&"-t".to_owned()));
        assert!(arguments.contains(&"--add-host=host.docker.internal:host-gateway".to_owned()));
        assert!(
            !arguments
                .iter()
                .any(|argument| argument.starts_with("--network="))
        );
        assert!(arguments.contains(&format!(
            "--defaults-file={MYSQL_OPTION_FILE_CONTAINER_PATH}"
        )));
        assert!(
            arguments
                .iter()
                .any(|argument| argument.ends_with(",readonly"))
        );
        assert!(!arguments.join(" ").contains("remote-test-password"));
    }

    #[tokio::test]
    async fn the_destination_is_reattested_then_created_if_missing_and_never_dropped() {
        let directory = tempfile::tempdir().unwrap();
        let artifact = validated_artifact(directory.path(), b"SELECT 1;\n").await;
        let arguments = arguments(&ensure_process_spec(
            &target(),
            artifact.metadata(),
            Path::new("/tmp/reprodb/client.cnf"),
            "reprodb-push-0123456789abcdef0123456789abcdef",
        ));
        let sql = arguments.last().unwrap();

        assert_eq!(arguments[arguments.len() - 2], "--execute");
        assert_eq!(
            sql,
            "DO IF(@@server_uuid = '33333333-3333-4333-8333-333333333333', 0, (SELECT 1 UNION ALL SELECT 2)); \
             CREATE DATABASE IF NOT EXISTS `salt_sagatec_qa` CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci;"
        );
        assert!(!sql.to_ascii_uppercase().contains("DROP"));
    }

    #[tokio::test]
    async fn the_import_uses_binary_mode_on_the_destination_database() {
        let directory = tempfile::tempdir().unwrap();
        let artifact = validated_artifact(directory.path(), b"SELECT 1;\n").await;
        let arguments = arguments(&import_process_spec(
            &target(),
            &artifact,
            Path::new("/tmp/reprodb/client.cnf"),
            "reprodb-push-0123456789abcdef0123456789abcdef",
        ));

        assert!(arguments.contains(&"--binary-mode".to_owned()));
        assert!(arguments.contains(&"--database=salt_sagatec_qa".to_owned()));
        assert!(arguments.contains(&"--default-character-set=utf8mb4".to_owned()));
    }

    #[test]
    fn failures_are_classified_without_confusing_privileges_and_passwords() {
        assert_eq!(
            classify_remote_failure(
                b"ERROR 1044 (42000): Access denied for user 'qa'@'%' to database 'salt_sagatec_qa'"
            ),
            RemoteFailureKind::Permission
        );
        assert_eq!(
            classify_remote_failure(
                b"ERROR 1045 (28000): Access denied for user 'qa'@'10.0.0.1' (using password: YES)"
            ),
            RemoteFailureKind::Authentication
        );
        assert_eq!(
            classify_remote_failure(b"ERROR 2003 (HY000): Can't connect to MySQL server"),
            RemoteFailureKind::DestinationUnavailable
        );
        assert_eq!(
            classify_remote_failure(
                b"ERROR 1242 (21000) at line 1: Subquery returns more than 1 row"
            ),
            RemoteFailureKind::DestinationChanged
        );
    }

    #[test]
    fn failures_explain_that_a_rerun_imports_over_a_partial_database() {
        let interrupted = RemoteImportError::from(RestoreExecutorError::Interrupted);
        let failed = RemoteImportError::ImportFailed {
            exit_code: Some(1),
            kind: RemoteFailureKind::Sql,
            stderr_truncated: false,
        };

        for error in [interrupted, failed] {
            let message = error.to_string();
            assert!(message.contains("partially imported"));
            assert!(message.contains("rerunning the same push"));
        }
    }
}
