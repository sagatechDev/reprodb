use std::{ffi::OsString, path::Path};

use secrecy::SecretString;
use thiserror::Error;

use crate::{
    domain::{ContainerId, DatabaseName, MysqlTlsMode, MysqlVersion},
    infrastructure::{
        credentials::{MYSQL_OPTION_FILE_CONTAINER_PATH, MysqlOptionFile, OptionFileError},
        mysql::{ApprovedMysqlClient, ClientCatalog, ClientCatalogError},
        process::{ProcessError, ProcessOutput, ProcessRunner, ProcessSpec},
    },
};

const IMAGE_DIGEST_TEMPLATE: &str = "{{json .RepoDigests}}";
const VERSION_QUERY: &str =
    "SELECT VERSION(), @@version_comment; SHOW SESSION STATUS LIKE 'Ssl_cipher'";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MysqlServerInfo {
    pub version: MysqlVersion,
    pub vendor: String,
    pub tls_cipher: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedMysqlClient {
    approved: ApprovedMysqlClient,
    docker_context: String,
}

impl PreparedMysqlClient {
    pub const fn approved(&self) -> ApprovedMysqlClient {
        self.approved
    }

    pub fn docker_context(&self) -> &str {
        &self.docker_context
    }
}

pub struct DockerMysqlClientRuntime<R> {
    runner: R,
}

impl<R> DockerMysqlClientRuntime<R>
where
    R: ProcessRunner,
{
    pub fn new(runner: R) -> Self {
        Self { runner }
    }

    pub async fn current_context(&self) -> Result<String, DockerClientError> {
        let output = self
            .runner
            .output(&ProcessSpec::new("docker").args(["context", "show"]))
            .await?;
        if !output.success {
            return Err(DockerClientError::DockerUnavailable);
        }
        let context = std::str::from_utf8(&output.stdout)
            .map_err(|_| DockerClientError::InvalidDockerContext)?
            .trim();
        validate_docker_context(context)?;
        Ok(context.to_owned())
    }

    pub async fn prepare(
        &self,
        docker_context: &str,
        mysql_series: &str,
        configured_image: &str,
    ) -> Result<PreparedMysqlClient, DockerClientError> {
        validate_docker_context(docker_context)?;
        let client = ClientCatalog::validate(mysql_series, configured_image)?;
        self.ensure_image(docker_context, client).await?;
        self.verify_client_version(docker_context, client).await?;
        Ok(PreparedMysqlClient {
            approved: client,
            docker_context: docker_context.to_owned(),
        })
    }

    pub async fn prepare_existing(
        &self,
        docker_context: &str,
        mysql_series: &str,
        configured_image: &str,
    ) -> Result<PreparedMysqlClient, DockerClientError> {
        validate_docker_context(docker_context)?;
        let client = ClientCatalog::validate(mysql_series, configured_image)?;
        let inspection = self
            .runner
            .output(&image_inspect_spec(docker_context, client))
            .await?;
        if !inspection.success {
            return if image_is_missing(&inspection.stderr) {
                Err(DockerClientError::ImageUnavailable)
            } else {
                Err(DockerClientError::DockerUnavailable)
            };
        }
        validate_inspected_digest(&inspection.stdout, client)?;
        self.verify_client_version(docker_context, client).await?;
        Ok(PreparedMysqlClient {
            approved: client,
            docker_context: docker_context.to_owned(),
        })
    }

    pub fn create_option_file(
        &self,
        source_host: &str,
        port: u16,
        username: &str,
        password: &SecretString,
        tls_mode: MysqlTlsMode,
    ) -> Result<MysqlOptionFile, DockerClientError> {
        MysqlOptionFile::create(
            docker_source_host(source_host),
            port,
            username,
            password,
            tls_mode,
        )
        .map_err(Into::into)
    }

    pub fn create_container_option_file(
        &self,
        port: u16,
        username: &str,
        password: &SecretString,
        tls_mode: MysqlTlsMode,
    ) -> Result<MysqlOptionFile, DockerClientError> {
        MysqlOptionFile::create("127.0.0.1", port, username, password, tls_mode).map_err(Into::into)
    }

    pub async fn probe_connection(
        &self,
        client: &PreparedMysqlClient,
        option_file: &MysqlOptionFile,
    ) -> Result<MysqlServerInfo, DockerClientError> {
        if !option_file.path().is_absolute() {
            return Err(DockerClientError::OptionFilePathNotAbsolute);
        }

        let output = self
            .runner
            .output(&connection_probe_spec(
                client.docker_context(),
                client.approved(),
                option_file.path(),
            ))
            .await?;
        if !output.success {
            return Err(classify_connection_failure(&output));
        }

        parse_server_info(&output.stdout)
    }

    pub async fn probe_container_connection(
        &self,
        client: &PreparedMysqlClient,
        container: &ContainerId,
        option_file: &MysqlOptionFile,
    ) -> Result<MysqlServerInfo, DockerClientError> {
        if !option_file.path().is_absolute() {
            return Err(DockerClientError::OptionFilePathNotAbsolute);
        }

        let output = self
            .runner
            .output(&container_connection_probe_spec(
                client.docker_context(),
                client.approved(),
                container,
                option_file.path(),
            ))
            .await?;
        if !output.success {
            return Err(classify_connection_failure(&output));
        }

        parse_server_info(&output.stdout)
    }

    pub(super) async fn query_connection(
        &self,
        client: &PreparedMysqlClient,
        option_file: &MysqlOptionFile,
        database: &DatabaseName,
        query: &str,
    ) -> Result<Vec<u8>, DockerClientError> {
        if !option_file.path().is_absolute() {
            return Err(DockerClientError::OptionFilePathNotAbsolute);
        }

        let output = self
            .runner
            .output(&connection_query_spec(
                client.docker_context(),
                client.approved(),
                option_file.path(),
                database,
                query,
            ))
            .await?;
        if !output.success {
            return Err(classify_query_failure(&output));
        }

        Ok(output.stdout)
    }

    async fn ensure_image(
        &self,
        docker_context: &str,
        client: ApprovedMysqlClient,
    ) -> Result<(), DockerClientError> {
        let first_inspection = self
            .runner
            .output(&image_inspect_spec(docker_context, client))
            .await?;

        let inspection = if first_inspection.success {
            first_inspection
        } else if image_is_missing(&first_inspection.stderr) {
            let pull = self
                .runner
                .output(&image_pull_spec(docker_context, client))
                .await?;
            if !pull.success {
                return Err(DockerClientError::ImagePullFailed);
            }

            let inspection = self
                .runner
                .output(&image_inspect_spec(docker_context, client))
                .await?;
            if !inspection.success {
                return Err(DockerClientError::ImageInspectFailed);
            }
            inspection
        } else {
            return Err(DockerClientError::DockerUnavailable);
        };

        validate_inspected_digest(&inspection.stdout, client)
    }

    async fn verify_client_version(
        &self,
        docker_context: &str,
        client: ApprovedMysqlClient,
    ) -> Result<(), DockerClientError> {
        let output = self
            .runner
            .output(&version_probe_spec(docker_context, client))
            .await?;
        if !output.success {
            return Err(DockerClientError::VersionProbeFailed);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let expected = client.version().to_string();
        let matches = stdout.split_whitespace().any(|part| {
            part.trim_matches(|character: char| {
                !character.is_ascii_alphanumeric() && character != '.'
            }) == expected
        });
        if !matches {
            return Err(DockerClientError::IncompatibleClientVersion);
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum DockerClientError {
    #[error(transparent)]
    Catalog(#[from] ClientCatalogError),

    #[error(transparent)]
    Process(#[from] ProcessError),

    #[error(transparent)]
    OptionFile(#[from] OptionFileError),

    #[error("the configured Docker context is invalid")]
    InvalidDockerContext,

    #[error(
        "Docker or its configured context is unavailable; start Docker and verify the selected context"
    )]
    DockerUnavailable,

    #[error(
        "the approved MySQL client image could not be pulled; verify registry and network access"
    )]
    ImagePullFailed,

    #[error("the approved MySQL client image is not available locally")]
    ImageUnavailable,

    #[error("the approved MySQL client image could not be inspected after pulling")]
    ImageInspectFailed,

    #[error("Docker returned invalid image metadata")]
    InvalidImageMetadata,

    #[error("the local MySQL client image digest differs from the approved digest")]
    ImageDigestMismatch,

    #[error("the approved MySQL client could not be started to verify its version")]
    VersionProbeFailed,

    #[error("the MySQL client binary version differs from the approved version")]
    IncompatibleClientVersion,

    #[error("the option file path must be absolute before it can be mounted into Docker")]
    OptionFilePathNotAbsolute,

    #[error(
        "the MySQL client container could not reach the source; verify Docker networking, VPN, host and port"
    )]
    SourceNetworkUnavailable,

    #[error("the MySQL source rejected the configured credential")]
    AuthenticationFailed,

    #[error("the MySQL connection probe failed; run `reprodb doctor` for diagnostics")]
    ConnectionProbeFailed,

    #[error("the MySQL read-only query failed; verify the expected source schema")]
    QueryFailed,

    #[error("the MySQL source returned invalid version metadata")]
    InvalidServerMetadata,
}

fn docker_source_host(source_host: &str) -> &str {
    match source_host {
        "127.0.0.1" | "localhost" | "::1" => "host.docker.internal",
        host => host,
    }
}

fn validate_docker_context(context: &str) -> Result<(), DockerClientError> {
    let valid = !context.is_empty()
        && context.len() <= 128
        && context
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'));
    if !valid {
        return Err(DockerClientError::InvalidDockerContext);
    }
    Ok(())
}

fn docker_spec(context: &str) -> ProcessSpec {
    ProcessSpec::new("docker").args(["--context", context])
}

fn image_inspect_spec(context: &str, client: ApprovedMysqlClient) -> ProcessSpec {
    docker_spec(context).args([
        "image",
        "inspect",
        "--format",
        IMAGE_DIGEST_TEMPLATE,
        client.image(),
    ])
}

fn image_pull_spec(context: &str, client: ApprovedMysqlClient) -> ProcessSpec {
    docker_spec(context).args(["image", "pull", "--quiet", client.image()])
}

fn version_probe_spec(context: &str, client: ApprovedMysqlClient) -> ProcessSpec {
    docker_spec(context).args([
        "run",
        "--rm",
        "--pull=never",
        client.image(),
        "mysql",
        "--version",
    ])
}

fn connection_probe_spec(
    context: &str,
    client: ApprovedMysqlClient,
    option_file: &Path,
) -> ProcessSpec {
    let mut mount = OsString::from("type=bind,src=");
    mount.push(option_file.as_os_str());
    mount.push(format!(",dst={MYSQL_OPTION_FILE_CONTAINER_PATH},readonly"));

    docker_spec(context).args([
        OsString::from("run"),
        OsString::from("--rm"),
        OsString::from("--pull=never"),
        OsString::from("--add-host=host.docker.internal:host-gateway"),
        OsString::from("--mount"),
        mount,
        OsString::from(client.image()),
        OsString::from("mysql"),
        OsString::from(format!(
            "--defaults-file={MYSQL_OPTION_FILE_CONTAINER_PATH}"
        )),
        OsString::from("--no-login-paths"),
        OsString::from("--batch"),
        OsString::from("--skip-column-names"),
        OsString::from("--execute"),
        OsString::from(VERSION_QUERY),
    ])
}

fn connection_query_spec(
    context: &str,
    client: ApprovedMysqlClient,
    option_file: &Path,
    database: &DatabaseName,
    query: &str,
) -> ProcessSpec {
    let mut mount = OsString::from("type=bind,src=");
    mount.push(option_file.as_os_str());
    mount.push(format!(",dst={MYSQL_OPTION_FILE_CONTAINER_PATH},readonly"));

    docker_spec(context).args([
        OsString::from("run"),
        OsString::from("--rm"),
        OsString::from("--pull=never"),
        OsString::from("--add-host=host.docker.internal:host-gateway"),
        OsString::from("--mount"),
        mount,
        OsString::from(client.image()),
        OsString::from("mysql"),
        OsString::from(format!(
            "--defaults-file={MYSQL_OPTION_FILE_CONTAINER_PATH}"
        )),
        OsString::from("--no-login-paths"),
        OsString::from("--batch"),
        OsString::from("--skip-column-names"),
        OsString::from(format!("--database={}", database.as_str())),
        OsString::from("--execute"),
        OsString::from(query),
    ])
}

fn container_connection_probe_spec(
    context: &str,
    client: ApprovedMysqlClient,
    container: &ContainerId,
    option_file: &Path,
) -> ProcessSpec {
    let mut mount = OsString::from("type=bind,src=");
    mount.push(option_file.as_os_str());
    mount.push(format!(",dst={MYSQL_OPTION_FILE_CONTAINER_PATH},readonly"));

    docker_spec(context).args([
        OsString::from("run"),
        OsString::from("--rm"),
        OsString::from("--pull=never"),
        OsString::from(format!("--network=container:{}", container.as_str())),
        OsString::from("--mount"),
        mount,
        OsString::from(client.image()),
        OsString::from("mysql"),
        OsString::from(format!(
            "--defaults-file={MYSQL_OPTION_FILE_CONTAINER_PATH}"
        )),
        OsString::from("--no-login-paths"),
        OsString::from("--batch"),
        OsString::from("--skip-column-names"),
        OsString::from("--execute"),
        OsString::from(VERSION_QUERY),
    ])
}

fn image_is_missing(stderr: &[u8]) -> bool {
    String::from_utf8_lossy(stderr)
        .to_ascii_lowercase()
        .contains("no such image")
}

fn validate_inspected_digest(
    stdout: &[u8],
    client: ApprovedMysqlClient,
) -> Result<(), DockerClientError> {
    let digests: Vec<String> =
        serde_json::from_slice(stdout).map_err(|_| DockerClientError::InvalidImageMetadata)?;
    if !digests
        .iter()
        .any(|digest| digest == client.repository_digest())
    {
        return Err(DockerClientError::ImageDigestMismatch);
    }
    Ok(())
}

fn classify_connection_failure(output: &ProcessOutput) -> DockerClientError {
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    if stderr.contains("access denied") {
        DockerClientError::AuthenticationFailed
    } else if [
        "can't connect",
        "unknown mysql server host",
        "connection refused",
        "network is unreachable",
        "no route to host",
        "temporary failure in name resolution",
    ]
    .iter()
    .any(|marker| stderr.contains(marker))
    {
        DockerClientError::SourceNetworkUnavailable
    } else {
        DockerClientError::ConnectionProbeFailed
    }
}

fn classify_query_failure(output: &ProcessOutput) -> DockerClientError {
    match classify_connection_failure(output) {
        DockerClientError::ConnectionProbeFailed => DockerClientError::QueryFailed,
        error => error,
    }
}

fn parse_server_info(stdout: &[u8]) -> Result<MysqlServerInfo, DockerClientError> {
    let output =
        std::str::from_utf8(stdout).map_err(|_| DockerClientError::InvalidServerMetadata)?;
    let mut lines = output.lines();
    let line = lines
        .next()
        .ok_or(DockerClientError::InvalidServerMetadata)?
        .trim_end_matches('\r');
    let (version, vendor) = line
        .split_once('\t')
        .ok_or(DockerClientError::InvalidServerMetadata)?;
    let version = version
        .parse()
        .map_err(|_| DockerClientError::InvalidServerMetadata)?;
    if vendor.is_empty() || vendor.chars().any(char::is_control) {
        return Err(DockerClientError::InvalidServerMetadata);
    }

    let tls_cipher = match lines.next() {
        Some(line) => {
            let line = line.trim_end_matches('\r');
            let (name, value) = line
                .split_once('\t')
                .ok_or(DockerClientError::InvalidServerMetadata)?;
            if name != "Ssl_cipher"
                || value.chars().count() > 128
                || value.chars().any(char::is_control)
            {
                return Err(DockerClientError::InvalidServerMetadata);
            }
            (!value.is_empty()).then(|| value.to_owned())
        }
        None => None,
    };
    if lines.any(|line| !line.is_empty()) {
        return Err(DockerClientError::InvalidServerMetadata);
    }

    Ok(MysqlServerInfo {
        version,
        vendor: vendor.to_owned(),
        tls_cipher,
    })
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, fs, sync::Mutex};

    use async_trait::async_trait;
    use secrecy::SecretString;

    use super::*;

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

    fn inspected_digest(client: ApprovedMysqlClient) -> ProcessOutput {
        ProcessOutput::success(serde_json::to_vec(&vec![client.repository_digest()]).unwrap())
    }

    fn arguments(spec: &ProcessSpec) -> Vec<String> {
        spec.arguments()
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    #[tokio::test]
    async fn accepts_an_existing_approved_image_and_exact_client_version() {
        let client = ClientCatalog::resolve("8.4").unwrap();
        let runner = FakeRunner::new([
            inspected_digest(client),
            ProcessOutput::success("mysql  Ver 8.4.4 for Linux on aarch64\n"),
        ]);
        let runtime = DockerMysqlClientRuntime::new(runner);

        assert_eq!(
            runtime
                .prepare("desktop-linux", "8.4", client.image())
                .await
                .unwrap()
                .approved(),
            client
        );
        let commands = runtime.runner.commands();
        assert_eq!(commands.len(), 2);
        assert_eq!(
            arguments(&commands[0])[0..4],
            ["--context", "desktop-linux", "image", "inspect"]
        );
        assert!(arguments(&commands[1]).contains(&"--pull=never".to_owned()));
    }

    #[tokio::test]
    async fn pulls_only_when_the_approved_image_is_missing() {
        let client = ClientCatalog::resolve("8.4").unwrap();
        let runner = FakeRunner::new([
            ProcessOutput::failure(1, "Error: No such image"),
            ProcessOutput::success(Vec::new()),
            inspected_digest(client),
            ProcessOutput::success("mysql  Ver 8.4.4 for Linux on x86_64\n"),
        ]);
        let runtime = DockerMysqlClientRuntime::new(runner);

        runtime
            .prepare("default", "8.4", client.image())
            .await
            .unwrap();

        let commands = runtime.runner.commands();
        assert_eq!(commands.len(), 4);
        assert_eq!(
            arguments(&commands[1]),
            [
                "--context",
                "default",
                "image",
                "pull",
                "--quiet",
                client.image()
            ]
        );
    }

    #[tokio::test]
    async fn read_only_preparation_never_pulls_a_missing_image() {
        let client = ClientCatalog::resolve("8.4").unwrap();
        let runtime = DockerMysqlClientRuntime::new(FakeRunner::new([ProcessOutput::failure(
            1,
            "Error: No such image",
        )]));

        let error = runtime
            .prepare_existing("default", "8.4", client.image())
            .await
            .unwrap_err();

        assert!(matches!(error, DockerClientError::ImageUnavailable));
        assert_eq!(runtime.runner.commands().len(), 1);
    }

    #[tokio::test]
    async fn distinguishes_docker_unavailable_from_a_missing_image() {
        let client = ClientCatalog::resolve("8.4").unwrap();
        let runner = FakeRunner::new([ProcessOutput::failure(
            1,
            "Cannot connect to the Docker daemon",
        )]);
        let runtime = DockerMysqlClientRuntime::new(runner);

        assert!(matches!(
            runtime.prepare("default", "8.4", client.image()).await,
            Err(DockerClientError::DockerUnavailable)
        ));
        assert_eq!(runtime.runner.commands().len(), 1);
    }

    #[tokio::test]
    async fn reports_registry_failure_after_a_missing_image() {
        let client = ClientCatalog::resolve("8.4").unwrap();
        let runner = FakeRunner::new([
            ProcessOutput::failure(1, "Error: No such image"),
            ProcessOutput::failure(1, "registry unavailable; sensitive-marker"),
        ]);
        let runtime = DockerMysqlClientRuntime::new(runner);

        let error = runtime
            .prepare("default", "8.4", client.image())
            .await
            .unwrap_err();

        assert!(matches!(error, DockerClientError::ImagePullFailed));
        assert!(!error.to_string().contains("sensitive-marker"));
        assert!(!format!("{error:?}").contains("sensitive-marker"));
    }

    #[tokio::test]
    async fn refuses_a_digest_mismatch_before_starting_a_client() {
        let client = ClientCatalog::resolve("8.4").unwrap();
        let runner = FakeRunner::new([ProcessOutput::success(
            b"[\"mysql@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"]"
                .to_vec(),
        )]);
        let runtime = DockerMysqlClientRuntime::new(runner);

        assert!(matches!(
            runtime.prepare("default", "8.4", client.image()).await,
            Err(DockerClientError::ImageDigestMismatch)
        ));
        assert_eq!(runtime.runner.commands().len(), 1);
    }

    #[tokio::test]
    async fn refuses_an_incompatible_binary_version() {
        let client = ClientCatalog::resolve("8.4").unwrap();
        let runner = FakeRunner::new([
            inspected_digest(client),
            ProcessOutput::success("mysql  Ver 8.0.45 for Linux on aarch64\n"),
        ]);
        let runtime = DockerMysqlClientRuntime::new(runner);

        assert!(matches!(
            runtime.prepare("default", "8.4", client.image()).await,
            Err(DockerClientError::IncompatibleClientVersion)
        ));
    }

    #[tokio::test]
    async fn discovers_and_validates_the_current_docker_context() {
        let runtime = DockerMysqlClientRuntime::new(FakeRunner::new([ProcessOutput::success(
            "desktop-linux\n",
        )]));

        assert_eq!(runtime.current_context().await.unwrap(), "desktop-linux");
        let commands = runtime.runner.commands();
        assert_eq!(arguments(&commands[0]), ["context", "show"]);
    }

    #[tokio::test]
    async fn rejects_untrusted_docker_context_output() {
        let runtime = DockerMysqlClientRuntime::new(FakeRunner::new([ProcessOutput::success(
            "default\n--host=unexpected",
        )]));

        assert!(matches!(
            runtime.current_context().await,
            Err(DockerClientError::InvalidDockerContext)
        ));
    }

    #[tokio::test]
    async fn connection_probe_mounts_the_option_file_read_only_without_shell() {
        let client = ClientCatalog::resolve("8.4").unwrap();
        let runner = FakeRunner::new([ProcessOutput::success(
            "8.4.4\tMySQL Community Server - GPL\n",
        )]);
        let runtime = DockerMysqlClientRuntime::new(runner);
        let option_file = runtime
            .create_option_file(
                "127.0.0.1",
                3306,
                "root",
                &SecretString::from("password-that-must-not-leak"),
                MysqlTlsMode::Preferred,
            )
            .unwrap();
        let option_contents = fs::read_to_string(option_file.path()).unwrap();
        assert!(option_contents.contains("host=\"host.docker.internal\""));
        assert!(!option_contents.contains("host=\"127.0.0.1\""));
        let prepared = PreparedMysqlClient {
            approved: client,
            docker_context: "desktop-linux".to_owned(),
        };

        let info = runtime
            .probe_connection(&prepared, &option_file)
            .await
            .unwrap();

        assert_eq!(info.version.to_string(), "8.4.4");
        let commands = runtime.runner.commands();
        let args = arguments(&commands[0]);
        let defaults_position = args
            .iter()
            .position(|argument| argument.starts_with("--defaults-file="))
            .unwrap();
        assert_eq!(args[defaults_position - 1], "mysql");
        assert!(args.iter().any(|argument| {
            argument.starts_with("type=bind,src=")
                && argument.ends_with("dst=/run/secrets/reprodb.cnf,readonly")
        }));
        assert!(args.contains(&"--add-host=host.docker.internal:host-gateway".to_owned()));
        assert!(args.contains(&"--pull=never".to_owned()));
        assert!(!args.join(" ").contains("password-that-must-not-leak"));
        assert!(
            !args
                .iter()
                .any(|argument| matches!(argument.as_str(), "sh" | "bash" | "-c"))
        );
    }

    #[tokio::test]
    async fn connection_errors_are_actionable_without_echoing_stderr() {
        let client = ClientCatalog::resolve("8.4").unwrap();
        let runner = FakeRunner::new([ProcessOutput::failure(
            1,
            "Can't connect to MySQL server; sensitive-marker",
        )]);
        let runtime = DockerMysqlClientRuntime::new(runner);
        let option_file = runtime
            .create_option_file(
                "host.docker.internal",
                3306,
                "root",
                &SecretString::from("secret"),
                MysqlTlsMode::Preferred,
            )
            .unwrap();
        let marker = "sensitive-marker";
        let prepared = PreparedMysqlClient {
            approved: client,
            docker_context: "default".to_owned(),
        };

        let error = runtime
            .probe_connection(&prepared, &option_file)
            .await
            .unwrap_err();

        assert!(matches!(error, DockerClientError::SourceNetworkUnavailable));
        assert!(!error.to_string().contains(marker));
        assert!(!format!("{error:?}").contains(marker));
    }

    #[tokio::test]
    async fn target_probe_joins_the_selected_container_network_by_exact_id() {
        let client = ClientCatalog::resolve("8.4").unwrap();
        let runtime = DockerMysqlClientRuntime::new(FakeRunner::new([ProcessOutput::success(
            "8.4.4\tMySQL Community Server - GPL\n",
        )]));
        let option_file = runtime
            .create_container_option_file(
                3306,
                "root",
                &SecretString::from("password-that-must-not-leak"),
                MysqlTlsMode::Required,
            )
            .unwrap();
        assert!(
            fs::read_to_string(option_file.path())
                .unwrap()
                .contains("host=\"127.0.0.1\"")
        );
        let prepared = PreparedMysqlClient {
            approved: client,
            docker_context: "desktop-linux".to_owned(),
        };
        let container = ContainerId::try_from("a".repeat(64)).unwrap();

        let server = runtime
            .probe_container_connection(&prepared, &container, &option_file)
            .await
            .unwrap();

        assert_eq!(server.version.to_string(), "8.4.4");
        let commands = runtime.runner.commands();
        let args = arguments(&commands[0]);
        assert!(args.contains(&format!("--network=container:{}", container.as_str())));
        assert!(!args.join(" ").contains("password-that-must-not-leak"));
        assert!(
            !args
                .iter()
                .any(|argument| matches!(argument.as_str(), "sh" | "bash" | "-c"))
        );
    }

    #[test]
    fn maps_only_loopback_hosts_into_the_docker_host_namespace() {
        for loopback in ["127.0.0.1", "localhost", "::1"] {
            assert_eq!(docker_source_host(loopback), "host.docker.internal");
        }
        assert_eq!(docker_source_host("db.internal"), "db.internal");
        assert_eq!(docker_source_host("10.0.0.8"), "10.0.0.8");
    }

    #[test]
    fn parses_the_negotiated_tls_cipher_without_accepting_extra_rows() {
        let info = parse_server_info(
            b"8.4.4\tMySQL Community Server - GPL\nSsl_cipher\tTLS_AES_256_GCM_SHA384\n",
        )
        .unwrap();

        assert_eq!(info.tls_cipher.as_deref(), Some("TLS_AES_256_GCM_SHA384"));
        assert!(matches!(
            parse_server_info(b"8.4.4\tMySQL\nSsl_cipher\tTLS_AES\nunexpected\n"),
            Err(DockerClientError::InvalidServerMetadata)
        ));
    }

    #[test]
    fn rejects_context_injection() {
        let error = validate_docker_context("default; docker rm").unwrap_err();
        assert!(matches!(error, DockerClientError::InvalidDockerContext));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    #[ignore = "requires a running local Docker Engine and may pull the approved image"]
    async fn native_docker_prepares_the_approved_client() {
        use crate::infrastructure::process::TokioProcessRunner;

        let client = ClientCatalog::resolve("8.4").unwrap();
        let context =
            std::env::var("REPRODB_TEST_DOCKER_CONTEXT").unwrap_or_else(|_| "default".to_owned());
        let runtime = DockerMysqlClientRuntime::new(TokioProcessRunner);

        let prepared = runtime
            .prepare(&context, client.series(), client.image())
            .await
            .unwrap();

        assert_eq!(prepared.approved(), client);
        assert_eq!(prepared.docker_context(), context);
    }
}
