use std::{
    ffi::OsString,
    fs::File,
    io::{self, BufReader, Read},
    process::Stdio,
};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    io::AsyncWriteExt,
    process::{Child, Command},
    sync::mpsc,
    task::JoinError,
};

use crate::{
    application::AuthorizedLocalTarget,
    domain::{DumpArtifactMetadata, Sha256Digest},
    infrastructure::{
        cancellation::CancellationToken,
        credentials::{MYSQL_OPTION_FILE_CONTAINER_PATH, MysqlOptionFile},
        docker::{ephemeral_container_name, terminate_ephemeral_run},
        process::{BoundedBytes, ProcessSpec, read_bounded},
        restore_artifact::ValidatedRestoreArtifact,
    },
};

const MAX_STDERR_BYTES: usize = 64 * 1024;
const STREAM_BUFFER_BYTES: usize = 64 * 1024;
const BUFFERED_CHUNKS: usize = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RestoreMetrics {
    imported_bytes: u64,
}

impl RestoreMetrics {
    pub const fn imported_bytes(self) -> u64 {
        self.imported_bytes
    }

    #[cfg(test)]
    pub(crate) const fn new_for_test(imported_bytes: u64) -> Self {
        Self { imported_bytes }
    }
}

#[async_trait]
pub trait RestoreExecutor: Send + Sync {
    async fn recreate_database(
        &self,
        target: &AuthorizedLocalTarget,
        metadata: &DumpArtifactMetadata,
    ) -> Result<(), RestoreExecutorError>;

    async fn import(
        &self,
        target: &AuthorizedLocalTarget,
        artifact: &ValidatedRestoreArtifact,
    ) -> Result<RestoreMetrics, RestoreExecutorError>;
}

#[derive(Clone, Debug, Default)]
pub struct DockerMysqlRestoreExecutor {
    cancellation: CancellationToken,
}

impl DockerMysqlRestoreExecutor {
    pub fn new(cancellation: CancellationToken) -> Self {
        Self { cancellation }
    }
}

#[async_trait]
impl RestoreExecutor for DockerMysqlRestoreExecutor {
    async fn recreate_database(
        &self,
        target: &AuthorizedLocalTarget,
        metadata: &DumpArtifactMetadata,
    ) -> Result<(), RestoreExecutorError> {
        if self.cancellation.is_cancelled() {
            return Err(RestoreExecutorError::Interrupted);
        }
        let option_file = target_option_file(target)?;
        let operation_container = ephemeral_container_name("restore");
        let spec =
            recreate_process_spec(target, metadata, option_file.path(), &operation_container);
        let (status, diagnostic) = run_without_stdin(
            &spec,
            &self.cancellation,
            target.docker_context(),
            &operation_container,
        )
        .await?;
        if !status.success() {
            return Err(RestoreExecutorError::RecreateFailed {
                exit_code: status.code(),
                kind: classify_failure(&diagnostic.bytes),
                stderr_truncated: diagnostic.truncated,
            });
        }
        Ok(())
    }

    async fn import(
        &self,
        target: &AuthorizedLocalTarget,
        artifact: &ValidatedRestoreArtifact,
    ) -> Result<RestoreMetrics, RestoreExecutorError> {
        if self.cancellation.is_cancelled() {
            return Err(RestoreExecutorError::Interrupted);
        }
        let option_file = target_option_file(target)?;
        let operation_container = ephemeral_container_name("restore");
        let spec = import_process_spec(target, artifact, option_file.path(), &operation_container);
        let mut child = spawn(&spec, true)?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or(RestoreExecutorError::MissingStdin)?;
        let stderr = child
            .stderr
            .take()
            .ok_or(RestoreExecutorError::MissingStderr)?;
        let stderr_task = tokio::spawn(read_bounded(stderr, MAX_STDERR_BYTES));

        let path = artifact.dump_path().to_owned();
        let (sender, mut receiver) = mpsc::channel::<Vec<u8>>(BUFFERED_CHUNKS);
        let decoder = tokio::task::spawn_blocking(move || decode_chunks(path, sender));
        let copy_result = async {
            loop {
                let chunk = tokio::select! {
                    biased;
                    () = self.cancellation.cancelled() => {
                        return Err(RestoreExecutorError::Interrupted);
                    }
                    chunk = receiver.recv() => chunk,
                };
                let Some(chunk) = chunk else {
                    break;
                };
                tokio::select! {
                    biased;
                    () = self.cancellation.cancelled() => {
                        return Err(RestoreExecutorError::Interrupted);
                    }
                    result = stdin.write_all(&chunk) => {
                        result.map_err(RestoreExecutorError::WriteStdin)?;
                    }
                }
            }
            tokio::select! {
                biased;
                () = self.cancellation.cancelled() => {
                    Err(RestoreExecutorError::Interrupted)
                }
                result = stdin.shutdown() => result.map_err(RestoreExecutorError::CloseStdin),
            }
        }
        .await;
        drop(receiver);
        drop(stdin);

        let mut interrupted_while_waiting = false;
        let status = if copy_result.is_err() {
            terminate_ephemeral_run(&mut child, target.docker_context(), &operation_container).await
        } else {
            tokio::select! {
                biased;
                () = self.cancellation.cancelled() => {
                    interrupted_while_waiting = true;
                    terminate_ephemeral_run(
                        &mut child,
                        target.docker_context(),
                        &operation_container,
                    ).await
                }
                status = child.wait() => status,
            }
        }
        .map_err(RestoreExecutorError::Wait);
        let diagnostic = stderr_task
            .await
            .map_err(RestoreExecutorError::StderrTask)??;
        let decoded = decoder.await.map_err(RestoreExecutorError::DecoderTask)?;

        if matches!(&copy_result, Err(RestoreExecutorError::Interrupted))
            || interrupted_while_waiting
        {
            if let Err(error) = status {
                tracing::warn!(%error, "could not confirm interrupted restore child termination");
            }
            return Err(RestoreExecutorError::Interrupted);
        }
        copy_result?;
        let decoded = decoded?;
        let status = status?;
        if !status.success() {
            return Err(RestoreExecutorError::ImportFailed {
                exit_code: status.code(),
                kind: classify_failure(&diagnostic.bytes),
                stderr_truncated: diagnostic.truncated,
            });
        }
        if decoded.bytes != artifact.metadata().uncompressed_bytes
            || decoded.sha256 != artifact.metadata().sql_sha256
        {
            return Err(RestoreExecutorError::ArtifactChangedDuringImport);
        }
        Ok(RestoreMetrics {
            imported_bytes: decoded.bytes,
        })
    }
}

fn target_option_file(
    target: &AuthorizedLocalTarget,
) -> Result<MysqlOptionFile, RestoreExecutorError> {
    MysqlOptionFile::create(
        "127.0.0.1",
        3306,
        target.username(),
        target.password(),
        crate::domain::MysqlTlsMode::Required,
    )
    .map_err(RestoreExecutorError::OptionFile)
}

fn recreate_process_spec(
    target: &AuthorizedLocalTarget,
    metadata: &DumpArtifactMetadata,
    option_file: &std::path::Path,
    operation_container: &str,
) -> ProcessSpec {
    let database = target.database().as_str();
    let sql = format!(
        "DROP DATABASE IF EXISTS `{database}`; CREATE DATABASE `{database}` CHARACTER SET {} COLLATE {};",
        metadata.database_charset, metadata.database_collation
    );
    mysql_process_spec(target, option_file, false, operation_container).args(["--execute", &sql])
}

fn import_process_spec(
    target: &AuthorizedLocalTarget,
    artifact: &ValidatedRestoreArtifact,
    option_file: &std::path::Path,
    operation_container: &str,
) -> ProcessSpec {
    mysql_process_spec(target, option_file, true, operation_container).args([
        OsString::from("--binary-mode"),
        OsString::from(format!("--database={}", target.database())),
        OsString::from(format!(
            "--default-character-set={}",
            artifact.metadata().database_charset
        )),
    ])
}

fn mysql_process_spec(
    target: &AuthorizedLocalTarget,
    option_file: &std::path::Path,
    interactive: bool,
    operation_container: &str,
) -> ProcessSpec {
    let mut mount = OsString::from("type=bind,src=");
    mount.push(option_file.as_os_str());
    mount.push(format!(",dst={MYSQL_OPTION_FILE_CONTAINER_PATH},readonly"));

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
    spec.args([
        OsString::from("--pull=never"),
        OsString::from(format!("--network=container:{}", target.container_id())),
        OsString::from("--mount"),
        mount,
        OsString::from(target.client().image()),
        OsString::from("mysql"),
        OsString::from(format!(
            "--defaults-file={MYSQL_OPTION_FILE_CONTAINER_PATH}"
        )),
        OsString::from("--no-login-paths"),
    ])
}

fn spawn(spec: &ProcessSpec, pipe_stdin: bool) -> Result<Child, RestoreExecutorError> {
    Command::new(spec.program())
        .args(spec.arguments())
        .stdin(if pipe_stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(RestoreExecutorError::Start)
}

async fn run_without_stdin(
    spec: &ProcessSpec,
    cancellation: &CancellationToken,
    docker_context: &str,
    operation_container: &str,
) -> Result<(std::process::ExitStatus, BoundedBytes), RestoreExecutorError> {
    let mut child = spawn(spec, false)?;
    let stderr = child
        .stderr
        .take()
        .ok_or(RestoreExecutorError::MissingStderr)?;
    let stderr_task = tokio::spawn(read_bounded(stderr, MAX_STDERR_BYTES));
    let status = tokio::select! {
        biased;
        () = cancellation.cancelled() => {
            terminate_ephemeral_run(&mut child, docker_context, operation_container)
                .await
                .map_err(RestoreExecutorError::Wait)?;
            let _ = stderr_task.await;
            return Err(RestoreExecutorError::Interrupted);
        }
        status = child.wait() => status.map_err(RestoreExecutorError::Wait)?,
    };
    let diagnostic = stderr_task
        .await
        .map_err(RestoreExecutorError::StderrTask)??;
    Ok((status, diagnostic))
}

struct DecodedInput {
    bytes: u64,
    sha256: Sha256Digest,
}

fn decode_chunks(
    path: std::path::PathBuf,
    sender: mpsc::Sender<Vec<u8>>,
) -> Result<DecodedInput, RestoreExecutorError> {
    let file = File::open(path).map_err(RestoreExecutorError::OpenArtifact)?;
    let mut decoder = zstd::stream::read::Decoder::new(BufReader::new(file))
        .map_err(RestoreExecutorError::DecodeArtifact)?;
    let mut buffer = vec![0_u8; STREAM_BUFFER_BYTES];
    let mut bytes = 0_u64;
    let mut hasher = Sha256::new();
    loop {
        let count = decoder
            .read(&mut buffer)
            .map_err(RestoreExecutorError::DecodeArtifact)?;
        if count == 0 {
            break;
        }
        bytes = bytes
            .checked_add(count as u64)
            .ok_or(RestoreExecutorError::ArtifactTooLarge)?;
        hasher.update(&buffer[..count]);
        sender
            .blocking_send(buffer[..count].to_vec())
            .map_err(|_| RestoreExecutorError::MysqlStoppedEarly)?;
    }
    Ok(DecodedInput {
        bytes,
        sha256: Sha256Digest::from_bytes(hasher.finalize().into()),
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestoreFailureKind {
    Authentication,
    Permission,
    TargetUnavailable,
    Sql,
    DockerUnavailable,
    Unknown,
}

fn classify_failure(stderr: &[u8]) -> RestoreFailureKind {
    let stderr = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    if stderr.contains("access denied") {
        RestoreFailureKind::Authentication
    } else if stderr.contains("permission denied")
        || stderr.contains("you need (at least one of) the")
    {
        RestoreFailureKind::Permission
    } else if stderr.contains("cannot connect to the docker daemon")
        || stderr.contains("error during connect")
    {
        RestoreFailureKind::DockerUnavailable
    } else if stderr.contains("can't connect") || stderr.contains("connection refused") {
        RestoreFailureKind::TargetUnavailable
    } else if stderr.contains("error ") || stderr.contains("unknown database") {
        RestoreFailureKind::Sql
    } else {
        RestoreFailureKind::Unknown
    }
}

#[derive(Debug, Error)]
pub enum RestoreExecutorError {
    #[error("could not create the private MySQL target credential file")]
    OptionFile(#[source] crate::infrastructure::credentials::OptionFileError),
    #[error("restore interrupted")]
    Interrupted,
    #[error("could not start the Dockerized mysql process")]
    Start(#[source] io::Error),
    #[error("could not wait for the Dockerized mysql process")]
    Wait(#[source] io::Error),
    #[error("the Dockerized mysql process did not expose stdin")]
    MissingStdin,
    #[error("the Dockerized mysql process did not expose stderr")]
    MissingStderr,
    #[error("could not read mysql diagnostics")]
    ReadStderr(#[from] io::Error),
    #[error("the mysql diagnostics task terminated unexpectedly")]
    StderrTask(#[source] JoinError),
    #[error("the dump decoder task terminated unexpectedly")]
    DecoderTask(#[source] JoinError),
    #[error("could not open the validated dump artifact")]
    OpenArtifact(#[source] io::Error),
    #[error("could not decode the validated Zstandard dump")]
    DecodeArtifact(#[source] io::Error),
    #[error("the dump is too large to count safely")]
    ArtifactTooLarge,
    #[error("mysql stopped before consuming the complete dump")]
    MysqlStoppedEarly,
    #[error("could not stream SQL into mysql")]
    WriteStdin(#[source] io::Error),
    #[error("could not close mysql stdin after the dump")]
    CloseStdin(#[source] io::Error),
    #[error("the validated dump changed while it was being imported")]
    ArtifactChangedDuringImport,
    #[error(
        "database recreation failed ({kind:?}, exit code {exit_code:?}, diagnostics truncated: {stderr_truncated})"
    )]
    RecreateFailed {
        exit_code: Option<i32>,
        kind: RestoreFailureKind,
        stderr_truncated: bool,
    },
    #[error(
        "database import failed ({kind:?}, exit code {exit_code:?}, diagnostics truncated: {stderr_truncated})"
    )]
    ImportFailed {
        exit_code: Option<i32>,
        kind: RestoreFailureKind,
        stderr_truncated: bool,
    },
}

#[cfg(test)]
mod tests {
    use crate::domain::{
        DatabaseEncoding, DatabaseName, DumpArtifactCompletion, DumpArtifactContext, DumpId,
        MysqlVersion, ProfileName, Sha256Digest, TenantId, TenantLookup,
    };

    use super::*;

    fn metadata(database: DatabaseName) -> DumpArtifactMetadata {
        DumpArtifactMetadata::try_new(
            DumpId::new(),
            DumpArtifactContext {
                tenant_lookup: TenantLookup::try_from("sagatec").unwrap(),
                tenant_id: TenantId::try_from("salt_sagatec").unwrap(),
                database,
                profile: ProfileName::try_from("local-source").unwrap(),
                source_fingerprint: Sha256Digest::from_bytes([1; 32]),
                source_server_uuid: "11111111-1111-4111-8111-111111111111".parse().unwrap(),
                source_version: "8.4.4".parse::<MysqlVersion>().unwrap(),
                client_version: "8.4.4".parse::<MysqlVersion>().unwrap(),
                database_encoding: DatabaseEncoding::try_new(
                    "utf8mb4".to_owned(),
                    "utf8mb4_0900_ai_ci".to_owned(),
                )
                .unwrap(),
                local_tenant_features: Default::default(),
                policy_version: 1,
            },
            DumpArtifactCompletion {
                created_at_unix_seconds: 100,
                completed_at_unix_seconds: 101,
                uncompressed_bytes: 10,
                compressed_bytes: 10,
                sql_sha256: Sha256Digest::from_bytes([2; 32]),
                artifact_sha256: Sha256Digest::from_bytes([3; 32]),
            },
        )
        .unwrap()
    }

    fn arguments(spec: &ProcessSpec) -> Vec<String> {
        spec.arguments()
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn streaming_client_uses_i_without_t_and_exact_container_network() {
        let target =
            AuthorizedLocalTarget::for_test(DatabaseName::try_from("salt_sagatec").unwrap());
        let spec = mysql_process_spec(
            &target,
            std::path::Path::new("/tmp/reprodb/client.cnf"),
            true,
            "reprodb-restore-0123456789abcdef0123456789abcdef",
        );
        let arguments = arguments(&spec);

        assert_eq!(spec.program(), "docker");
        assert!(arguments.contains(&"-i".to_owned()));
        assert!(!arguments.contains(&"-t".to_owned()));
        assert!(arguments.contains(&format!("--network=container:{}", target.container_id())));
        assert!(arguments.contains(&target.client().image().to_owned()));
        assert!(arguments.contains(&"--name".to_owned()));
        assert!(arguments.contains(&"reprodb-restore-0123456789abcdef0123456789abcdef".to_owned()));
        assert!(!arguments.join(" ").contains("local-test-password"));
    }

    #[test]
    fn recreation_sql_uses_only_validated_database_encoding_and_identifier() {
        let database = DatabaseName::try_from("salt_sagatec").unwrap();
        let target = AuthorizedLocalTarget::for_test(database.clone());
        let spec = recreate_process_spec(
            &target,
            &metadata(database),
            std::path::Path::new("/tmp/reprodb/client.cnf"),
            "reprodb-restore-0123456789abcdef0123456789abcdef",
        );
        let arguments = arguments(&spec);
        let sql = arguments.last().unwrap();

        assert_eq!(arguments[arguments.len() - 2], "--execute");
        assert_eq!(
            sql,
            "DROP DATABASE IF EXISTS `salt_sagatec`; CREATE DATABASE `salt_sagatec` CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci;"
        );
    }

    #[test]
    fn failure_classification_never_returns_diagnostics() {
        let marker = "password-that-must-not-leak";
        assert_eq!(
            classify_failure(format!("Access denied for {marker}").as_bytes()),
            RestoreFailureKind::Authentication
        );
        assert!(!format!("{:?}", classify_failure(marker.as_bytes())).contains(marker));
    }
}
