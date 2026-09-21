use std::{ffi::OsString, io, process::Stdio, sync::Arc};

use async_trait::async_trait;
use secrecy::SecretString;
use thiserror::Error;
use tokio::{process::Command, task::JoinError};

use crate::{
    domain::{ApprovedDumpPlan, ProfileName},
    infrastructure::{
        cancellation::CancellationToken,
        compression::{CompressionError, CompressionMetrics, ZstdCompressor},
        config::SourceProfileConfig,
        credentials::{MYSQL_OPTION_FILE_CONTAINER_PATH, MYSQL_SECRETS_CONTAINER_DIRECTORY},
        docker::{ephemeral_container_name, terminate_ephemeral_run},
        mysql::{ApprovedMysqlClient, DockerClientError, DockerMysqlClientRuntime},
        process::{ProcessSpec, TokioProcessRunner, read_bounded},
    },
};

const MAX_STDERR_BYTES: usize = 64 * 1024;

pub struct DumpExecutionRequest<'a> {
    pub profile_name: &'a ProfileName,
    pub docker_context: &'a str,
    pub profile: &'a SourceProfileConfig,
    pub password: &'a SecretString,
    pub client: ApprovedMysqlClient,
    pub plan: &'a ApprovedDumpPlan,
    pub progress: Arc<dyn crate::infrastructure::compression::CompressionProgressObserver>,
}

#[async_trait]
pub trait DumpExecutor: Send + Sync {
    async fn execute(
        &self,
        request: DumpExecutionRequest<'_>,
        output: std::fs::File,
    ) -> Result<CompressionMetrics, DumpExecutorError>;
}

#[derive(Clone, Debug)]
pub struct DockerMysqlDumpExecutor {
    cancellation: CancellationToken,
    compression_level: i32,
}

impl Default for DockerMysqlDumpExecutor {
    fn default() -> Self {
        Self::new(CancellationToken::default())
    }
}

impl DockerMysqlDumpExecutor {
    pub fn new(cancellation: CancellationToken) -> Self {
        Self {
            cancellation,
            compression_level: crate::infrastructure::compression::DEFAULT_ZSTD_LEVEL,
        }
    }

    pub fn with_compression_level(mut self, compression_level: i32) -> Self {
        self.compression_level = compression_level;
        self
    }
}

#[async_trait]
impl DumpExecutor for DockerMysqlDumpExecutor {
    async fn execute(
        &self,
        request: DumpExecutionRequest<'_>,
        output: std::fs::File,
    ) -> Result<CompressionMetrics, DumpExecutorError> {
        if self.cancellation.is_cancelled() {
            return Err(DumpExecutorError::Interrupted);
        }
        let runtime = DockerMysqlClientRuntime::new(TokioProcessRunner);
        let option_file = runtime.create_option_file_with_tls_material(
            &request.profile.host,
            request.profile.port,
            &request.profile.username,
            request.password,
            request.profile.tls_mode,
            &request.profile.tls_material,
        )?;
        let operation_container = ephemeral_container_name("dump");
        let spec = dump_process_spec(&request, option_file.path(), &operation_container);
        let mut child = Command::new(spec.program())
            .args(spec.arguments())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(DumpExecutorError::Start)?;
        let stdout = child
            .stdout
            .take()
            .ok_or(DumpExecutorError::MissingStdout)?;
        let stderr = child
            .stderr
            .take()
            .ok_or(DumpExecutorError::MissingStderr)?;
        let stderr_task = tokio::spawn(read_bounded(stderr, MAX_STDERR_BYTES));

        let compression = ZstdCompressor::new(self.compression_level)
            .compress_with_cancellation(
                stdout,
                output,
                Arc::clone(&request.progress),
                self.cancellation.clone(),
            )
            .await;
        let mut interrupted_while_waiting = false;
        let status = if compression.is_err() {
            terminate_ephemeral_run(&mut child, request.docker_context, &operation_container).await
        } else {
            tokio::select! {
                biased;
                () = self.cancellation.cancelled() => {
                    interrupted_while_waiting = true;
                    terminate_ephemeral_run(
                        &mut child,
                        request.docker_context,
                        &operation_container,
                    ).await
                }
                status = child.wait() => status,
            }
        }
        .map_err(DumpExecutorError::Wait);
        let diagnostic = stderr_task.await.map_err(DumpExecutorError::StderrTask)??;

        let metrics = match compression {
            Err(CompressionError::Interrupted) => {
                if let Err(error) = status {
                    tracing::warn!(%error, "could not confirm interrupted dump child termination");
                }
                return Err(DumpExecutorError::Interrupted);
            }
            result => result?,
        };
        if interrupted_while_waiting {
            if let Err(error) = status {
                tracing::warn!(%error, "could not confirm interrupted dump child termination");
            }
            return Err(DumpExecutorError::Interrupted);
        }
        let status = status?;
        if !status.success() {
            return Err(DumpExecutorError::ProcessFailed {
                exit_code: status.code(),
                kind: classify_failure(&diagnostic.bytes),
                stderr_truncated: diagnostic.truncated,
            });
        }
        Ok(metrics)
    }
}

fn dump_process_spec(
    request: &DumpExecutionRequest<'_>,
    option_file: &std::path::Path,
    operation_container: &str,
) -> ProcessSpec {
    let mut mount = OsString::from("type=bind,src=");
    mount.push(option_file.parent().unwrap_or(option_file).as_os_str());
    mount.push(format!(",dst={MYSQL_SECRETS_CONTAINER_DIRECTORY},readonly"));

    let mut spec = ProcessSpec::new("docker")
        .args(["--context", request.docker_context, "run", "--rm", "--name"])
        .args([
            OsString::from(operation_container),
            OsString::from("--pull=never"),
            OsString::from("--add-host=host.docker.internal:host-gateway"),
            OsString::from("--mount"),
            mount,
            OsString::from(request.client.image()),
            OsString::from("mysqldump"),
            OsString::from(format!(
                "--defaults-file={MYSQL_OPTION_FILE_CONTAINER_PATH}"
            )),
        ]);
    if request.client.supports_no_login_paths() {
        spec = spec.arg("--no-login-paths");
    }
    spec.args(request.plan.arguments().iter().map(OsString::from))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DumpFailureKind {
    Authentication,
    Permission,
    DatabaseUnavailable,
    SourceUnavailable,
    DockerUnavailable,
    Unknown,
}

impl std::fmt::Display for DumpFailureKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Authentication => {
                "the source rejected authentication; verify the profile with `reprodb doctor`"
            }
            Self::Permission => {
                "the source user lacks a privilege required by the approved dump policy"
            }
            Self::DatabaseUnavailable => {
                "the resolved tenant database no longer exists or is not visible to this user"
            }
            Self::SourceUnavailable => {
                "the MySQL source became unavailable; verify network or VPN connectivity"
            }
            Self::DockerUnavailable => {
                "Docker became unavailable; start Docker and run `reprodb doctor`"
            }
            Self::Unknown => {
                "the MySQL client returned an unclassified failure; run `reprodb doctor`"
            }
        })
    }
}

fn classify_failure(stderr: &[u8]) -> DumpFailureKind {
    let stderr = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    if stderr.contains("access denied") {
        DumpFailureKind::Authentication
    } else if stderr.contains("permission denied")
        || stderr.contains("you need (at least one of) the")
    {
        DumpFailureKind::Permission
    } else if stderr.contains("unknown database") {
        DumpFailureKind::DatabaseUnavailable
    } else if stderr.contains("cannot connect to the docker daemon")
        || stderr.contains("error during connect")
    {
        DumpFailureKind::DockerUnavailable
    } else if stderr.contains("can't connect")
        || stderr.contains("cannot connect")
        || stderr.contains("connection refused")
        || stderr.contains("connection timed out")
        || stderr.contains("lost connection")
    {
        DumpFailureKind::SourceUnavailable
    } else {
        DumpFailureKind::Unknown
    }
}

#[derive(Debug, Error)]
pub enum DumpExecutorError {
    #[error(transparent)]
    Client(#[from] DockerClientError),

    #[error(transparent)]
    Compression(#[from] CompressionError),

    #[error("dump interrupted")]
    Interrupted,

    #[error("could not start the Dockerized mysqldump process")]
    Start(#[source] io::Error),

    #[error("could not wait for the Dockerized mysqldump process")]
    Wait(#[source] io::Error),

    #[error("the Dockerized mysqldump process did not expose stdout")]
    MissingStdout,

    #[error("the Dockerized mysqldump process did not expose stderr")]
    MissingStderr,

    #[error("could not read mysqldump diagnostics")]
    ReadStderr(#[from] io::Error),

    #[error("the mysqldump diagnostics task terminated unexpectedly")]
    StderrTask(#[source] JoinError),

    #[error(
        "mysqldump failed: {kind} (exit code: {exit_code:?}, diagnostics truncated: {stderr_truncated})"
    )]
    ProcessFailed {
        exit_code: Option<i32>,
        kind: DumpFailureKind,
        stderr_truncated: bool,
    },
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use secrecy::SecretString;

    use crate::{
        domain::{
            DatabaseEncoding, DatabaseName, DatabaseObjectCounts, DefinerObjectCounts,
            DumpPreflight, GtidMode, Mysql8DumpPolicy, MysqlVersion, ProfileName,
            StorageEngineUsage,
        },
        infrastructure::{
            compression::NoCompressionProgress,
            config::{MysqlClientConfig, MysqlFamily, SourceProfileConfig},
            mysql::ClientCatalog,
        },
    };

    use super::*;

    fn request<'a>(
        profile_name: &'a ProfileName,
        profile: &'a SourceProfileConfig,
        password: &'a SecretString,
        plan: &'a crate::domain::ApprovedDumpPlan,
    ) -> DumpExecutionRequest<'a> {
        DumpExecutionRequest {
            profile_name,
            docker_context: "desktop-linux",
            profile,
            password,
            client: ClientCatalog::resolve("8.4").unwrap(),
            plan,
            progress: Arc::new(NoCompressionProgress),
        }
    }

    fn profile() -> SourceProfileConfig {
        SourceProfileConfig {
            host: "127.0.0.1".to_owned(),
            port: 3306,
            username: "root".to_owned(),
            credential_key: "source:550e8400-e29b-41d4-a716-446655440000"
                .parse()
                .unwrap(),
            mysql_family: MysqlFamily::Mysql,
            mysql_series: "8.4".to_owned(),
            production: false,
            tls_mode: crate::domain::MysqlTlsMode::Disabled,
            tls_material: Default::default(),
            client: MysqlClientConfig {
                image: ClientCatalog::resolve("8.4").unwrap().image().to_owned(),
            },
        }
    }

    fn plan() -> crate::domain::ApprovedDumpPlan {
        Mysql8DumpPolicy::evaluate(
            "8.4.4".parse::<MysqlVersion>().unwrap(),
            "MySQL Community Server - GPL",
            "8.4.4".parse::<MysqlVersion>().unwrap(),
            &DatabaseName::try_from("acme_production").unwrap(),
            &DumpPreflight {
                encoding: DatabaseEncoding::try_new(
                    "utf8mb4".to_owned(),
                    "utf8mb4_0900_ai_ci".to_owned(),
                )
                .unwrap(),
                estimated_data_bytes: 1024,
                engines: vec![StorageEngineUsage::try_new("InnoDB".to_owned(), 1).unwrap()],
                objects: DatabaseObjectCounts::default(),
                definers: DefinerObjectCounts::default(),
                gtid_mode: GtidMode::On,
            },
        )
        .unwrap()
    }

    #[test]
    fn command_is_structured_and_mounts_the_secret_without_putting_it_in_argv() {
        let profile = profile();
        let profile_name = ProfileName::try_from("local-source").unwrap();
        let password = SecretString::from("password-that-must-not-leak");
        let plan = plan();
        let option_path = std::path::Path::new("/tmp/reprodb/client.cnf");
        let spec = dump_process_spec(
            &request(&profile_name, &profile, &password, &plan),
            option_path,
            "reprodb-dump-0123456789abcdef0123456789abcdef",
        );
        let arguments = spec
            .arguments()
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(spec.program(), "docker");
        assert_eq!(
            &arguments[..14],
            [
                "--context",
                "desktop-linux",
                "run",
                "--rm",
                "--name",
                "reprodb-dump-0123456789abcdef0123456789abcdef",
                "--pull=never",
                "--add-host=host.docker.internal:host-gateway",
                "--mount",
                "type=bind,src=/tmp/reprodb,dst=/run/secrets/reprodb,readonly",
                ClientCatalog::resolve("8.4").unwrap().image(),
                "mysqldump",
                "--defaults-file=/run/secrets/reprodb/client.cnf",
                "--no-login-paths",
            ]
        );
        assert!(arguments.ends_with(plan.arguments()));
        assert!(!arguments.join(" ").contains("password-that-must-not-leak"));
        assert!(!arguments.iter().any(|argument| argument == "-c"));
    }

    #[test]
    fn stderr_is_classified_without_becoming_part_of_the_error() {
        let marker = "password-that-must-not-leak";
        assert_eq!(
            classify_failure(format!("Access denied for {marker}").as_bytes()),
            DumpFailureKind::Authentication
        );
        assert_eq!(
            classify_failure(b"Cannot connect to the Docker daemon"),
            DumpFailureKind::DockerUnavailable
        );
        assert_eq!(
            classify_failure(b"You need (at least one of) the PROCESS privilege(s)"),
            DumpFailureKind::Permission
        );
        assert_eq!(
            classify_failure(b"Lost connection to MySQL server during query"),
            DumpFailureKind::SourceUnavailable
        );
        let error = DumpExecutorError::ProcessFailed {
            exit_code: Some(2),
            kind: DumpFailureKind::Authentication,
            stderr_truncated: false,
        };
        assert!(error.to_string().contains("reprodb doctor"));
        assert!(!error.to_string().contains(marker));
        assert!(!format!("{error:?}").contains(marker));
    }
}
