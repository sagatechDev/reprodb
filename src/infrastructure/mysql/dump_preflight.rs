use secrecy::SecretString;
use thiserror::Error;

use crate::{
    domain::{
        ApprovedDumpPlan, DatabaseEncoding, DatabaseName, DatabaseObjectCounts,
        DefinerObjectCounts, DumpPolicyError, DumpPreflight, DumpPreflightMetadataError, GtidMode,
        Mysql8DumpPolicy, MysqlTlsMode, StorageEngineUsage,
    },
    infrastructure::{
        mysql::{
            ApprovedMysqlClient, DockerClientError, DockerMysqlClientRuntime, MysqlServerInfo,
        },
        process::ProcessRunner,
    },
};

const MAX_PREFLIGHT_OUTPUT_BYTES: usize = 16 * 1024;

const PREFLIGHT_QUERY: &str = "\
SELECT 'DATABASE', HEX(default_character_set_name), HEX(default_collation_name) \
FROM information_schema.schemata WHERE BINARY schema_name = BINARY DATABASE();\
SELECT 'ENGINE', HEX(COALESCE(engine, '')), COUNT(*) \
FROM information_schema.tables \
WHERE BINARY table_schema = BINARY DATABASE() AND table_type = 'BASE TABLE' \
GROUP BY engine ORDER BY engine;\
SELECT 'OBJECTS', \
  (SELECT COUNT(*) FROM information_schema.views WHERE BINARY table_schema = BINARY DATABASE()), \
  (SELECT COUNT(*) FROM information_schema.triggers WHERE BINARY trigger_schema = BINARY DATABASE()), \
  (SELECT COUNT(*) FROM information_schema.routines WHERE BINARY routine_schema = BINARY DATABASE()), \
  (SELECT COUNT(*) FROM information_schema.events WHERE BINARY event_schema = BINARY DATABASE());\
SELECT 'DEFINERS', \
  (SELECT COUNT(*) FROM information_schema.views WHERE BINARY table_schema = BINARY DATABASE() AND definer <> ''), \
  (SELECT COUNT(*) FROM information_schema.triggers WHERE BINARY trigger_schema = BINARY DATABASE() AND definer <> ''), \
  (SELECT COUNT(*) FROM information_schema.routines WHERE BINARY routine_schema = BINARY DATABASE() AND definer <> ''), \
  (SELECT COUNT(*) FROM information_schema.events WHERE BINARY event_schema = BINARY DATABASE() AND definer <> '');\
SELECT 'GTID', @@GLOBAL.gtid_mode";

pub struct DumpPreflightSource {
    pub docker_context: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: SecretString,
    pub tls_mode: MysqlTlsMode,
    pub client: ApprovedMysqlClient,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovedMysqlDump {
    pub server: MysqlServerInfo,
    pub preflight: DumpPreflight,
    pub plan: ApprovedDumpPlan,
}

pub struct DockerMysqlDumpPreflight<R> {
    runtime: DockerMysqlClientRuntime<R>,
    source: DumpPreflightSource,
}

impl<R> DockerMysqlDumpPreflight<R>
where
    R: ProcessRunner,
{
    pub fn new(runner: R, source: DumpPreflightSource) -> Self {
        Self {
            runtime: DockerMysqlClientRuntime::new(runner),
            source,
        }
    }

    pub async fn assess(
        &self,
        database: &DatabaseName,
    ) -> Result<ApprovedMysqlDump, DumpPreflightError> {
        let prepared = self
            .runtime
            .prepare_existing(
                &self.source.docker_context,
                self.source.client.series(),
                self.source.client.image(),
            )
            .await?;
        let option_file = self.runtime.create_option_file(
            &self.source.host,
            self.source.port,
            &self.source.username,
            &self.source.password,
            self.source.tls_mode,
        )?;
        let server = self
            .runtime
            .probe_connection(&prepared, &option_file)
            .await?;
        let output = self
            .runtime
            .query_connection(&prepared, &option_file, database, PREFLIGHT_QUERY)
            .await?;
        let preflight = parse_preflight(&output)?;
        let plan = Mysql8DumpPolicy::evaluate(
            server.version,
            &server.vendor,
            prepared.approved().version(),
            database,
            &preflight,
        )?;

        Ok(ApprovedMysqlDump {
            server,
            preflight,
            plan,
        })
    }
}

#[derive(Debug, Error)]
pub enum DumpPreflightError {
    #[error(transparent)]
    Client(#[from] DockerClientError),

    #[error(transparent)]
    Metadata(#[from] DumpPreflightMetadataError),

    #[error(transparent)]
    Policy(#[from] DumpPolicyError),

    #[error("the source returned incomplete or malformed dump preflight metadata")]
    InvalidMetadata,
}

fn parse_preflight(output: &[u8]) -> Result<DumpPreflight, DumpPreflightError> {
    if output.len() > MAX_PREFLIGHT_OUTPUT_BYTES {
        return Err(DumpPreflightError::InvalidMetadata);
    }
    let output = std::str::from_utf8(output).map_err(|_| DumpPreflightError::InvalidMetadata)?;
    let mut encoding = None;
    let mut engines = Vec::new();
    let mut objects = None;
    let mut definers = None;
    let mut gtid_mode = None;

    for line in output.lines() {
        let columns = line.trim_end_matches('\r').split('\t').collect::<Vec<_>>();
        match columns.as_slice() {
            ["DATABASE", charset, collation] if encoding.is_none() => {
                encoding = Some(DatabaseEncoding::try_new(
                    decode_hex(charset, 64)?,
                    decode_hex(collation, 64)?,
                )?);
            }
            ["ENGINE", engine, count] => {
                engines.push(StorageEngineUsage::try_new(
                    decode_hex(engine, 64)?,
                    parse_count(count)?,
                )?);
            }
            ["OBJECTS", views, triggers, routines, events] if objects.is_none() => {
                objects = Some(DatabaseObjectCounts {
                    views: parse_count(views)?,
                    triggers: parse_count(triggers)?,
                    routines: parse_count(routines)?,
                    events: parse_count(events)?,
                });
            }
            ["DEFINERS", views, triggers, routines, events] if definers.is_none() => {
                definers = Some(DefinerObjectCounts {
                    views: parse_count(views)?,
                    triggers: parse_count(triggers)?,
                    routines: parse_count(routines)?,
                    events: parse_count(events)?,
                });
            }
            ["GTID", mode] if gtid_mode.is_none() => {
                gtid_mode = Some(GtidMode::parse(mode)?);
            }
            _ => return Err(DumpPreflightError::InvalidMetadata),
        }
    }

    Ok(DumpPreflight {
        encoding: encoding.ok_or(DumpPreflightError::InvalidMetadata)?,
        engines,
        objects: objects.ok_or(DumpPreflightError::InvalidMetadata)?,
        definers: definers.ok_or(DumpPreflightError::InvalidMetadata)?,
        gtid_mode: gtid_mode.ok_or(DumpPreflightError::InvalidMetadata)?,
    })
}

fn parse_count(value: &str) -> Result<u64, DumpPreflightError> {
    value
        .parse()
        .map_err(|_| DumpPreflightError::InvalidMetadata)
}

fn decode_hex(value: &str, max_decoded_bytes: usize) -> Result<String, DumpPreflightError> {
    if !value.len().is_multiple_of(2) || value.len() > max_decoded_bytes * 2 {
        return Err(DumpPreflightError::InvalidMetadata);
    }
    let mut decoded = Vec::with_capacity(value.len() / 2);
    for pair in value.as_bytes().chunks_exact(2) {
        let high = hex_digit(pair[0]).ok_or(DumpPreflightError::InvalidMetadata)?;
        let low = hex_digit(pair[1]).ok_or(DumpPreflightError::InvalidMetadata)?;
        decoded.push((high << 4) | low);
    }
    String::from_utf8(decoded).map_err(|_| DumpPreflightError::InvalidMetadata)
}

fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        process::{Command, Stdio},
        sync::{Arc, Mutex},
    };

    use async_trait::async_trait;
    use secrecy::zeroize::Zeroize;

    use crate::infrastructure::{
        mysql::ClientCatalog,
        process::{ProcessError, ProcessOutput, ProcessSpec},
    };

    use super::*;

    struct FakeRunner {
        outputs: Mutex<VecDeque<ProcessOutput>>,
        commands: Arc<Mutex<Vec<ProcessSpec>>>,
    }

    #[async_trait]
    impl ProcessRunner for FakeRunner {
        async fn output(&self, spec: &ProcessSpec) -> Result<ProcessOutput, ProcessError> {
            self.commands.lock().unwrap().push(spec.clone());
            Ok(self.outputs.lock().unwrap().pop_front().unwrap())
        }
    }

    fn assessor(
        query_output: impl Into<Vec<u8>>,
    ) -> (
        DockerMysqlDumpPreflight<FakeRunner>,
        Arc<Mutex<Vec<ProcessSpec>>>,
    ) {
        let client = ClientCatalog::resolve("8.4").unwrap();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let runner = FakeRunner {
            outputs: Mutex::new(VecDeque::from([
                ProcessOutput::success(
                    serde_json::to_vec(&vec![client.repository_digest()]).unwrap(),
                ),
                ProcessOutput::success("mysql  Ver 8.4.4 for Linux on aarch64\n"),
                ProcessOutput::success(
                    "8.4.4\tMySQL Community Server - GPL\t11111111-1111-4111-8111-111111111111\nSsl_cipher\tTLS_AES_256_GCM_SHA384\n",
                ),
                ProcessOutput::success(query_output),
            ])),
            commands: Arc::clone(&commands),
        };
        (
            DockerMysqlDumpPreflight::new(
                runner,
                DumpPreflightSource {
                    docker_context: "desktop-linux".to_owned(),
                    host: "127.0.0.1".to_owned(),
                    port: 3306,
                    username: "readonly".to_owned(),
                    password: SecretString::from("password-marker"),
                    tls_mode: MysqlTlsMode::Required,
                    client,
                },
            ),
            commands,
        )
    }

    fn valid_output() -> &'static str {
        "DATABASE\t757466386D6234\t757466386D62345F303930305F61695F6369\n\
         ENGINE\t496E6E6F4442\t42\n\
         OBJECTS\t2\t1\t0\t0\n\
         DEFINERS\t2\t1\t0\t0\n\
         GTID\tOFF\n"
    }

    #[tokio::test]
    async fn observes_metadata_and_approves_a_structured_dump_plan() {
        let (assessor, commands) = assessor(valid_output());

        let approved = assessor
            .assess(&DatabaseName::try_from("salt_sagatec").unwrap())
            .await
            .unwrap();

        assert_eq!(approved.server.version.to_string(), "8.4.4");
        assert_eq!(approved.preflight.encoding.charset(), "utf8mb4");
        assert_eq!(
            approved.preflight.encoding.collation(),
            "utf8mb4_0900_ai_ci"
        );
        assert_eq!(approved.preflight.objects.views, 2);
        assert_eq!(approved.preflight.definers.triggers, 1);
        assert_eq!(approved.preflight.gtid_mode, GtidMode::Off);
        assert_eq!(approved.plan.arguments().last().unwrap(), "salt_sagatec");

        let commands = commands.lock().unwrap();
        let query_arguments = commands.last().unwrap().arguments();
        assert!(
            query_arguments
                .iter()
                .any(|argument| argument == "--database=salt_sagatec")
        );
        assert_eq!(query_arguments.last().unwrap(), PREFLIGHT_QUERY);
        assert!(
            !query_arguments
                .iter()
                .any(|argument| argument.to_string_lossy().contains("password-marker"))
        );
    }

    #[tokio::test]
    async fn policy_rejection_happens_after_read_only_observation_and_before_dump() {
        let output = valid_output().replace("496E6E6F4442\t42", "4D794953414D\t2");
        let (assessor, commands) = assessor(output);

        let error = assessor
            .assess(&DatabaseName::try_from("salt_polymer").unwrap())
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            DumpPreflightError::Policy(DumpPolicyError::NonTransactionalTables { count: 2 })
        ));
        assert_eq!(commands.lock().unwrap().len(), 4);
    }

    #[test]
    fn parses_an_empty_database_and_rejects_missing_duplicate_or_untrusted_metadata() {
        let empty = "DATABASE\t757466386D6234\t757466386D62345F62696E\n\
                     OBJECTS\t0\t0\t0\t0\n\
                     DEFINERS\t0\t0\t0\t0\n\
                     GTID\tON\n";
        assert!(
            parse_preflight(empty.as_bytes())
                .unwrap()
                .engines
                .is_empty()
        );

        for invalid in [
            "OBJECTS\t0\t0\t0\t0\nDEFINERS\t0\t0\t0\t0\nGTID\tOFF\n",
            "DATABASE\t757466386D6234\t757466386D62345F62696E\nDATABASE\t757466386D6234\t757466386D62345F62696E\nOBJECTS\t0\t0\t0\t0\nDEFINERS\t0\t0\t0\t0\nGTID\tOFF\n",
            "DATABASE\t757466386D62343B\t757466386D62345F62696E\nOBJECTS\t0\t0\t0\t0\nDEFINERS\t0\t0\t0\t0\nGTID\tOFF\n",
            "DATABASE\t757466386D6234\t757466386D62345F62696E\nOBJECTS\t0\t0\t0\t0\nDEFINERS\t0\t0\t0\t0\nGTID\tOFF;unsafe\n",
        ] {
            assert!(parse_preflight(invalid.as_bytes()).is_err(), "{invalid:?}");
        }
    }

    #[tokio::test]
    #[ignore = "requires the local mysql-8 Docker fixture on port 3306"]
    async fn preflights_the_local_salt_sagatec_database_read_only() {
        let docker_context = "desktop-linux";
        let client = ClientCatalog::resolve("8.4").unwrap();
        let password = local_container_root_password(docker_context, "mysql-8");
        let assessor = DockerMysqlDumpPreflight::new(
            crate::infrastructure::process::TokioProcessRunner,
            DumpPreflightSource {
                docker_context: docker_context.to_owned(),
                host: "127.0.0.1".to_owned(),
                port: 3306,
                username: "root".to_owned(),
                password,
                tls_mode: MysqlTlsMode::Disabled,
                client,
            },
        );

        let approved = assessor
            .assess(&DatabaseName::try_from("salt_sagatec").unwrap())
            .await
            .unwrap();

        assert_eq!(approved.server.version.to_string(), "8.4.4");
        assert!(!approved.preflight.engines.is_empty());
        assert!(
            approved
                .preflight
                .engines
                .iter()
                .all(|usage| usage.engine().eq_ignore_ascii_case("InnoDB"))
        );
        assert_eq!(
            approved.plan.arguments().last().map(String::as_str),
            Some("salt_sagatec")
        );
    }

    fn local_container_root_password(context: &str, container: &str) -> SecretString {
        let output = Command::new("docker")
            .args([
                "--context",
                context,
                "container",
                "inspect",
                "--format",
                "{{range .Config.Env}}{{println .}}{{end}}",
                container,
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .expect("Docker must be available for this ignored integration test");
        assert!(output.status.success(), "could not inspect test container");

        let mut environment = String::from_utf8(output.stdout)
            .expect("the test container environment must be valid UTF-8");
        let password = environment
            .lines()
            .find_map(|line| line.strip_prefix("MYSQL_ROOT_PASSWORD="))
            .map(str::to_owned)
            .expect("test container does not expose MYSQL_ROOT_PASSWORD");
        environment.zeroize();

        SecretString::from(password)
    }
}
