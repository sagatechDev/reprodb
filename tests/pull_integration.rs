#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::process::{Command, Stdio};

use reprodb::{
    application::{
        PullCacheUse, PullDatabaseSelectionError, PullDatabaseSelector, PullDumpDependencies,
        PullRestoreDependencies, PullService, PullTargetChoice, PullTargetSelectionError,
        PullTargetSelector,
    },
    domain::{CredentialKey, CredentialScope, DatabaseName, MysqlTlsMode, ProfileName},
    infrastructure::{
        config::{
            AppConfig, AppPaths, ClientRuntimeConfig, ConfigRepository, LocalTargetConfig,
            LocalTargetTrust, MysqlClientConfig, MysqlFamily, SourceProfileConfig,
        },
        credentials::{
            CredentialStore, MYSQL_OPTION_FILE_CONTAINER_PATH, MYSQL_SECRETS_CONTAINER_DIRECTORY,
            MemoryCredentialStore, MysqlOptionFile,
        },
        docker::DockerTargetDiscovery,
        mysql::{
            ClientCatalog, DockerDumpWorkflow, DockerLocalTargetAttestor, DockerMysqlDumpExecutor,
            DockerMysqlRestoreExecutor,
        },
        process::TokioProcessRunner,
    },
};
use secrecy::{ExposeSecret, SecretString};
use tempfile::TempDir;
use uuid::Uuid;

struct SourceDatabase;

impl PullDatabaseSelector for SourceDatabase {
    fn select(
        &self,
        source_database: &DatabaseName,
    ) -> Result<DatabaseName, PullDatabaseSelectionError> {
        Ok(source_database.clone())
    }
}

struct DefaultTarget;

impl PullTargetSelector for DefaultTarget {
    fn select(
        &self,
        choices: &[PullTargetChoice],
    ) -> Result<reprodb::domain::ContainerName, PullTargetSelectionError> {
        choices
            .iter()
            .find(|choice| choice.is_default)
            .map(|choice| choice.container.clone())
            .ok_or(PullTargetSelectionError)
    }
}

#[tokio::test]
#[ignore = "creates isolated MySQL source/target containers and proves pull plus cache reuse"]
async fn pulls_a_real_database_then_reuses_cache_without_the_source_credential() {
    let (context, _) = DockerTargetDiscovery::new(TokioProcessRunner)
        .discover()
        .await
        .unwrap();
    let suffix = Uuid::new_v4().simple().to_string();
    let source_name = format!("reprodb-source-{}", &suffix[..12]);
    let target_name = format!("reprodb-target-{}", &suffix[..12]);
    let source_password = SecretString::from("reprodb-integration-source-only");
    let dump_password = SecretString::from("reprodb-integration-readonly-only");
    let target_password = SecretString::from("reprodb-integration-target-only");
    let image = ClientCatalog::resolve("8.4").unwrap().image();
    let _source_guard =
        start_mysql_container(&context, &source_name, image, &source_password, true, false);
    let _target_guard =
        start_mysql_container(&context, &target_name, image, &target_password, false, true);
    let (_, candidates) = DockerTargetDiscovery::new(TokioProcessRunner)
        .discover()
        .await
        .unwrap();
    let source_candidate = candidates
        .iter()
        .find(|candidate| candidate.name.as_str() == source_name)
        .cloned()
        .expect("the temporary source MySQL container was not discovered");
    let source_port = source_candidate
        .published_ports
        .first()
        .expect("the source MySQL port was not published")
        .host_port;
    let target_candidate = candidates
        .into_iter()
        .find(|candidate| candidate.name.as_str() == target_name)
        .expect("the temporary target MySQL container was not discovered");
    let _database = DatabaseName::try_from(format!("salt_reprodb_pull_{suffix}")).unwrap();
    let database = DatabaseName::try_from(format!("reprodb_pull_{}", &suffix[..16])).unwrap();
    let benchmark_rows = benchmark_row_count();
    let profile_name = ProfileName::try_from("pull-local-source").unwrap();
    let client = ClientCatalog::resolve("8.4").unwrap();
    let temp = TempDir::new().unwrap();
    let paths = AppPaths::new(
        temp.path().join("config"),
        temp.path().join("cache"),
        temp.path().join("data"),
    );
    let repository = ConfigRepository::new(paths);
    let source_key = CredentialKey::new(CredentialScope::Source);
    let target_key = CredentialKey::new(CredentialScope::Target);
    repository
        .save(&AppConfig {
            active_profile: Some(profile_name.clone()),
            client_runtime: ClientRuntimeConfig {
                docker_context: Some(context.clone()),
                ..ClientRuntimeConfig::default()
            },
            local_target: Some(LocalTargetConfig {
                docker_context: context.clone(),
                container_name: target_candidate.name.clone(),
                container_id: target_candidate.id.clone(),
                username: "root".to_owned(),
                credential_key: target_key,
                trust: if target_candidate.managed_by_reprodb {
                    LocalTargetTrust::ReprodbManaged
                } else {
                    LocalTargetTrust::UserConfirmed
                },
            }),
            profiles: std::collections::BTreeMap::from([(
                profile_name,
                SourceProfileConfig {
                    host: "127.0.0.1".to_owned(),
                    port: source_port,
                    username: "reprodb_reader".to_owned(),
                    credential_key: source_key,
                    mysql_family: MysqlFamily::Mysql,
                    mysql_series: "8.4".to_owned(),
                    production: false,
                    tls_mode: MysqlTlsMode::Required,
                    tls_material: Default::default(),
                    client: MysqlClientConfig {
                        image: client.image().to_owned(),
                    },
                },
            )]),
            ..AppConfig::default()
        })
        .unwrap();
    let credentials = MemoryCredentialStore::default();
    credentials
        .set(&source_key, dump_password.clone())
        .await
        .unwrap();
    credentials
        .set(&target_key, target_password.clone())
        .await
        .unwrap();

    wait_for_mysql(
        &context,
        source_candidate.id.as_str(),
        client.image(),
        &source_password,
    )
    .await;
    wait_for_mysql(
        &context,
        target_candidate.id.as_str(),
        client.image(),
        &target_password,
    )
    .await;

    let fixture = run_mysql_query(
        &context,
        source_candidate.id.as_str(),
        client.image(),
        &source_password,
        MysqlTlsMode::Required,
        None,
        &format!(
            "CREATE DATABASE `{database}` CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci; \
             CREATE TABLE `{database}`.`reprodb_pull_items` (`id` BIGINT NOT NULL PRIMARY KEY, `label` VARCHAR(255) NOT NULL, `payload` MEDIUMTEXT NOT NULL) ENGINE=InnoDB; \
             CREATE TABLE `{database}`.`reprodb_pull_details` (`item_id` BIGINT NOT NULL PRIMARY KEY, `amount` DECIMAL(12,2) NOT NULL, `occurred_at` DATETIME NOT NULL, `binary_payload` BLOB NOT NULL, `nullable_note` VARCHAR(255) NULL, CONSTRAINT `reprodb_pull_details_item_fk` FOREIGN KEY (`item_id`) REFERENCES `reprodb_pull_items` (`id`)) ENGINE=InnoDB; \
             INSERT INTO `{database}`.`reprodb_pull_items` VALUES (1, CONVERT(0x5361676174656320F09FA782 USING utf8mb4), 'small-fixture-one'), (2, 'Polymer', 'small-fixture-two'); \
             INSERT INTO `{database}`.`reprodb_pull_details` VALUES (1, 1234567890.12, '2026-09-07 12:34:56', 0x0001FEFF, NULL);",
        ),
    );
    let generated_fixture = fixture.and_then(|_| {
        if benchmark_rows == 2 {
            Ok(Vec::new())
        } else {
            run_mysql_query(
                &context,
                source_candidate.id.as_str(),
                client.image(),
                &source_password,
                MysqlTlsMode::Required,
                Some(&database),
                &generated_rows_sql(benchmark_rows),
            )
        }
    });
    let logical_payload_bytes = generated_fixture.as_ref().ok().and_then(|_| {
        run_mysql_query(
            &context,
            source_candidate.id.as_str(),
            client.image(),
            &source_password,
            MysqlTlsMode::Required,
            Some(&database),
            "SELECT COALESCE(SUM(OCTET_LENGTH(payload)), 0) FROM reprodb_pull_items",
        )
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
    });
    let least_privilege_user = if generated_fixture.is_ok() {
        run_mysql_query(
            &context,
            source_candidate.id.as_str(),
            client.image(),
            &source_password,
            MysqlTlsMode::Required,
            None,
            &format!(
                "CREATE USER 'reprodb_reader'@'%' IDENTIFIED BY '{}';  \
                 GRANT SELECT, SHOW VIEW, TRIGGER ON `{database}`.* TO 'reprodb_reader'@'%'",
                dump_password.expose_secret()
            ),
        )
    } else {
        Err("source fixture failed before least-privilege user creation".to_owned())
    };

    let workflow = DockerDumpWorkflow::new(TokioProcessRunner);
    let attestor = DockerLocalTargetAttestor::new(TokioProcessRunner);
    let compression_level = benchmark_compression_level();
    let dump_executor =
        DockerMysqlDumpExecutor::default().with_compression_level(compression_level);
    let service = PullService::new(repository);
    let first = match (generated_fixture, least_privilege_user) {
        (Ok(_), Ok(_)) => service
            .pull(
                &credentials,
                PullDumpDependencies {
                    preflight: &workflow,
                    executor: &dump_executor,
                },
                PullRestoreDependencies {
                    target_attestor: &attestor,
                    executor: DockerMysqlRestoreExecutor::default(),
                    target_selector: &DefaultTarget,
                    database_selector: &SourceDatabase,
                },
                database.clone(),
                false,
            )
            .await
            .map_err(|error| error.to_string()),
        (Err(error), _) => Err(error),
        (_, Err(error)) => Err(error),
    };
    let source_credential_removed = credentials.delete(&source_key).await;
    let second = if first.is_ok() && source_credential_removed.is_ok() {
        service
            .pull(
                &credentials,
                PullDumpDependencies {
                    preflight: &workflow,
                    executor: &dump_executor,
                },
                PullRestoreDependencies {
                    target_attestor: &attestor,
                    executor: DockerMysqlRestoreExecutor::default(),
                    target_selector: &DefaultTarget,
                    database_selector: &SourceDatabase,
                },
                database.clone(),
                false,
            )
            .await
            .map_err(|error| error.to_string())
    } else {
        Err("first pull or source credential cleanup failed".to_owned())
    };
    let verification = if second.is_ok() {
        Some(run_mysql_query(
            &context,
            target_candidate.id.as_str(),
            client.image(),
            &target_password,
            MysqlTlsMode::Required,
            Some(&database),
            "SELECT id, HEX(label) FROM reprodb_pull_items WHERE id <= 2 ORDER BY id",
        ))
    } else {
        None
    };
    let row_count_verification = if second.is_ok() {
        Some(run_mysql_query(
            &context,
            target_candidate.id.as_str(),
            client.image(),
            &target_password,
            MysqlTlsMode::Required,
            Some(&database),
            "SELECT COUNT(*) FROM reprodb_pull_items",
        ))
    } else {
        None
    };
    let type_and_fk_verification = if second.is_ok() {
        Some(run_mysql_query(
            &context,
            target_candidate.id.as_str(),
            client.image(),
            &target_password,
            MysqlTlsMode::Required,
            Some(&database),
            "SELECT amount, DATE_FORMAT(occurred_at, '%Y-%m-%d %H:%i:%s'), HEX(binary_payload), IF(nullable_note IS NULL, 'NULL', nullable_note) FROM reprodb_pull_details WHERE item_id = 1; \
             SELECT COUNT(*) FROM information_schema.REFERENTIAL_CONSTRAINTS WHERE BINARY CONSTRAINT_SCHEMA = BINARY DATABASE() AND CONSTRAINT_NAME = 'reprodb_pull_details_item_fk'",
        ))
    } else {
        None
    };

    source_credential_removed.expect("the source credential must be removable before cache hit");
    let first = first.expect("the cache miss must create and restore a real dump");
    let second = second.expect("the cache hit must restore without a source credential");
    assert_eq!(first.cache, PullCacheUse::Created);
    assert!(matches!(second.cache, PullCacheUse::Hit { .. }));
    assert_eq!(first.restored.plan.dump_id, second.restored.plan.dump_id);
    assert_eq!(first.restored.plan.database, database);
    assert_eq!(first.restored.plan.container, target_candidate.name);
    let dump_metrics = first
        .metrics
        .dump
        .expect("a cache miss must report dump metrics");
    eprintln!(
        "rows={benchmark_rows} zstd_level={compression_level} logical_payload_bytes={} dump_input_bytes={} compressed_bytes={} ratio={:.4} dump_seconds={:.3} restore_seconds={:.3} total_seconds={:.3} cache_hit_seconds={:.3}",
        logical_payload_bytes.unwrap_or_default(),
        dump_metrics.uncompressed_bytes,
        dump_metrics.compressed_bytes,
        dump_metrics.compressed_bytes as f64 / dump_metrics.uncompressed_bytes as f64,
        dump_metrics.elapsed.as_secs_f64(),
        first.metrics.restore_elapsed.as_secs_f64(),
        first.metrics.total_elapsed.as_secs_f64(),
        second.metrics.total_elapsed.as_secs_f64(),
    );
    assert_eq!(
        String::from_utf8(verification.unwrap().unwrap()).unwrap(),
        "1\t5361676174656320F09FA782\n2\t506F6C796D6572\n"
    );
    assert_eq!(
        String::from_utf8(row_count_verification.unwrap().unwrap()).unwrap(),
        format!("{benchmark_rows}\n")
    );
    assert_eq!(
        String::from_utf8(type_and_fk_verification.unwrap().unwrap()).unwrap(),
        "1234567890.12\t2026-09-07 12:34:56\t0001FEFF\tNULL\n1\n"
    );
}

fn benchmark_row_count() -> u64 {
    let Some(value) = std::env::var_os("REPRODB_BENCHMARK_ROWS") else {
        return 2;
    };
    let rows = value
        .to_str()
        .and_then(|value| value.parse::<u64>().ok())
        .expect("REPRODB_BENCHMARK_ROWS must be an integer");
    assert!(
        (2..=500_000).contains(&rows),
        "REPRODB_BENCHMARK_ROWS must be between 2 and 500000"
    );
    rows
}

fn benchmark_compression_level() -> i32 {
    let Some(value) = std::env::var_os("REPRODB_BENCHMARK_ZSTD_LEVEL") else {
        return reprodb::infrastructure::compression::DEFAULT_ZSTD_LEVEL;
    };
    let level = value
        .to_str()
        .and_then(|value| value.parse::<i32>().ok())
        .expect("REPRODB_BENCHMARK_ZSTD_LEVEL must be an integer");
    assert!(
        matches!(level, 1 | 3),
        "REPRODB_BENCHMARK_ZSTD_LEVEL must be 1 or 3"
    );
    level
}

fn generated_rows_sql(rows: u64) -> String {
    let highest_sequence = rows - 1;
    let digit = "(SELECT 0 n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9)";
    format!(
        "INSERT INTO reprodb_pull_items (id, label, payload) \
         SELECT sequence + 1, CONCAT('generated-', sequence + 1), \
                CONCAT(SHA2(CONCAT(sequence, 'a'), 256), SHA2(CONCAT(sequence, 'b'), 256), \
                       SHA2(CONCAT(sequence, 'c'), 256), SHA2(CONCAT(sequence, 'd'), 256), \
                       SHA2(CONCAT(sequence, 'e'), 256), SHA2(CONCAT(sequence, 'f'), 256), \
                       SHA2(CONCAT(sequence, 'g'), 256), SHA2(CONCAT(sequence, 'h'), 256)) \
         FROM (SELECT ones.n + tens.n * 10 + hundreds.n * 100 + thousands.n * 1000 + \
                      ten_thousands.n * 10000 + hundred_thousands.n * 100000 AS sequence \
               FROM {digit} ones CROSS JOIN {digit} tens CROSS JOIN {digit} hundreds \
               CROSS JOIN {digit} thousands CROSS JOIN {digit} ten_thousands \
               CROSS JOIN {digit} hundred_thousands) AS generated_rows \
         WHERE sequence BETWEEN 2 AND {highest_sequence}"
    )
}

#[tokio::test]
#[ignore = "creates isolated MySQL containers and executes the real CLI across processes"]
async fn real_cli_configures_pulls_and_reuses_cache_with_the_source_offline() {
    let (context, _) = DockerTargetDiscovery::new(TokioProcessRunner)
        .discover()
        .await
        .unwrap();
    let suffix = Uuid::new_v4().simple().to_string();
    let source_name = format!("reprodb-cli-source-{}", &suffix[..12]);
    let target_name = format!("reprodb-cli-target-{}", &suffix[..12]);
    let source_password = SecretString::from("reprodb-cli-source-password");
    let target_password = SecretString::from("reprodb-cli-target-password");
    let client = ClientCatalog::resolve("8.4").unwrap();
    let _source_guard = start_mysql_container(
        &context,
        &source_name,
        client.image(),
        &source_password,
        true,
        false,
    );
    let _target_guard = start_mysql_container(
        &context,
        &target_name,
        client.image(),
        &target_password,
        false,
        true,
    );
    let (_, candidates) = DockerTargetDiscovery::new(TokioProcessRunner)
        .discover()
        .await
        .unwrap();
    let source = candidates
        .iter()
        .find(|candidate| candidate.name.as_str() == source_name)
        .cloned()
        .expect("the temporary CLI source must be discovered");
    let source_port = source
        .published_ports
        .first()
        .expect("the CLI source port must be published")
        .host_port;
    let target = candidates
        .iter()
        .find(|candidate| candidate.name.as_str() == target_name)
        .cloned()
        .expect("the temporary CLI target must be discovered");

    wait_for_mysql(
        &context,
        source.id.as_str(),
        client.image(),
        &source_password,
    )
    .await;
    wait_for_mysql(
        &context,
        target.id.as_str(),
        client.image(),
        &target_password,
    )
    .await;

    let database = DatabaseName::try_from(format!("cli_e2e_{suffix}")).unwrap();
    run_mysql_query(
        &context,
        source.id.as_str(),
        client.image(),
        &source_password,
        MysqlTlsMode::Required,
        None,
        &format!(
            "CREATE DATABASE `{database}` CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci; \
             CREATE TABLE `{database}`.`cli_items` (`id` BIGINT NOT NULL PRIMARY KEY, `label` VARCHAR(255) NOT NULL) ENGINE=InnoDB; \
             INSERT INTO `{database}`.`cli_items` VALUES (1, 'Sagatec'), (2, 'Polymer');",
        ),
    )
    .expect("the CLI source fixture must be created");

    let home = TempDir::new().unwrap();
    let credential_dir = home.path().join("test-credentials");
    let setup = run_real_cli(
        home.path(),
        &credential_dir,
        &[
            "setup",
            "--non-interactive",
            "--target",
            &target_name,
            "--password-stdin",
            "--yes",
            "--color",
            "never",
        ],
        Some(target_password.expose_secret()),
    );
    assert_cli_success("setup", &setup);

    let source_port = source_port.to_string();
    let profile = run_real_cli(
        home.path(),
        &credential_dir,
        &[
            "profile",
            "add",
            "cli-source",
            "--non-interactive",
            "--host",
            "127.0.0.1",
            "--port",
            &source_port,
            "--username",
            "root",
            "--tls",
            "required",
            "--password-stdin",
            "--color",
            "never",
        ],
        Some(source_password.expose_secret()),
    );
    assert_cli_success("profile add", &profile);

    let pull_arguments = [
        "pull",
        database.as_str(),
        "--target",
        &target_name,
        "--database",
        database.as_str(),
        "--color",
        "never",
    ];
    let first = run_real_cli(home.path(), &credential_dir, &pull_arguments, None);
    assert_cli_success("first pull", &first);
    let first_stdout = String::from_utf8_lossy(&first.stdout);
    assert!(first_stdout.contains("No matching cached dump was found."));
    assert!(first_stdout.contains("Cache:      new dump"));

    let stopped = Command::new("docker")
        .args(["--context", &context, "container", "stop", &source_name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .expect("Docker must stop the CLI source");
    assert!(
        stopped.success(),
        "the CLI source must stop before cache hit"
    );

    let second = run_real_cli(home.path(), &credential_dir, &pull_arguments, None);
    assert_cli_success("cached pull", &second);
    let second_stdout = String::from_utf8_lossy(&second.stdout);
    assert!(second_stdout.contains("Cached dump found"));
    assert!(second_stdout.contains("Cache:      reused"));

    let restored = run_mysql_query(
        &context,
        target.id.as_str(),
        client.image(),
        &target_password,
        MysqlTlsMode::Required,
        Some(&database),
        "SELECT id, label FROM cli_items ORDER BY id",
    )
    .expect("the restored CLI target must be queryable");
    assert_eq!(
        String::from_utf8(restored).unwrap(),
        "1\tSagatec\n2\tPolymer\n"
    );
}

fn run_real_cli(
    home: &std::path::Path,
    credential_dir: &std::path::Path,
    arguments: &[&str],
    stdin_secret: Option<&str>,
) -> std::process::Output {
    use std::io::Write as _;

    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("reprodb"))
        .args(arguments)
        .env("REPRODB_HOME", home)
        .env("REPRODB_TEST_CREDENTIAL_DIR", credential_dir)
        .stdin(if stdin_secret.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the real reprodb binary must start");
    if let Some(secret) = stdin_secret {
        let mut stdin = child.stdin.take().expect("CLI stdin must be piped");
        stdin
            .write_all(format!("{secret}\n").as_bytes())
            .expect("the test password must reach stdin");
    }
    let output = child
        .wait_with_output()
        .expect("the real reprodb binary must finish");
    for secret in ["reprodb-cli-source-password", "reprodb-cli-target-password"] {
        assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
    }
    output
}

fn assert_cli_success(operation: &str, output: &std::process::Output) {
    assert!(
        output.status.success(),
        "{operation} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

struct TemporaryDockerContainer {
    context: String,
    name: String,
}

impl Drop for TemporaryDockerContainer {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args([
                "--context",
                &self.context,
                "container",
                "rm",
                "--force",
                &self.name,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn start_mysql_container(
    context: &str,
    name: &str,
    image: &str,
    password: &SecretString,
    publish_source_port: bool,
    managed_target: bool,
) -> TemporaryDockerContainer {
    let mut command = Command::new("docker");
    command
        .env("MYSQL_ROOT_PASSWORD", password.expose_secret())
        .args([
            "--context",
            context,
            "run",
            "--detach",
            "--rm",
            "--pull=missing",
            "--name",
            name,
            "--env",
            "MYSQL_ROOT_PASSWORD",
        ]);
    if publish_source_port {
        command.args(["--publish", "127.0.0.1::3306"]);
    }
    if managed_target {
        command.args(["--label", "com.sagatech.reprodb.target=true"]);
    }
    let status = command
        .arg(image)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .expect("Docker must start the temporary target container");
    assert!(status.success(), "could not start temporary MySQL target");
    TemporaryDockerContainer {
        context: context.to_owned(),
        name: name.to_owned(),
    }
}

async fn wait_for_mysql(
    context: &str,
    container_id: &str,
    client_image: &str,
    password: &SecretString,
) {
    let mut last_error = None;
    for _ in 0..30 {
        match run_mysql_query(
            context,
            container_id,
            client_image,
            password,
            MysqlTlsMode::Required,
            None,
            "SELECT 1",
        ) {
            Ok(_) => return,
            Err(error) => last_error = Some(error),
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    panic!(
        "temporary MySQL target did not become ready: {}",
        last_error.unwrap_or_else(|| "no diagnostic".to_owned())
    );
}

fn run_mysql_query(
    context: &str,
    container_id: &str,
    client_image: &str,
    password: &SecretString,
    tls_mode: MysqlTlsMode,
    database: Option<&DatabaseName>,
    query: &str,
) -> Result<Vec<u8>, String> {
    let option_file = MysqlOptionFile::create("127.0.0.1", 3306, "root", password, tls_mode)
        .map_err(|error| error.to_string())?;
    let mount = format!(
        "type=bind,src={},dst={MYSQL_SECRETS_CONTAINER_DIRECTORY},readonly",
        option_file.directory_path().display()
    );
    let mut command = Command::new("docker");
    command.args([
        "--context",
        context,
        "run",
        "--rm",
        "--pull=never",
        &format!("--network=container:{container_id}"),
        "--mount",
        &mount,
        client_image,
        "mysql",
        &format!("--defaults-file={MYSQL_OPTION_FILE_CONTAINER_PATH}"),
        "--no-login-paths",
        "--batch",
        "--skip-column-names",
    ]);
    if let Some(database) = database {
        command.arg(format!("--database={database}"));
    }
    let output = command
        .args(["--execute", query])
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(String::from_utf8_lossy(&output.stderr).into_owned())
    }
}
