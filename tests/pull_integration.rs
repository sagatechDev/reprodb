#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::process::{Command, Stdio};

use reprodb::{
    application::{
        PullCacheUse, PullDatabaseSelectionError, PullDatabaseSelector, PullDumpDependencies,
        PullRestoreDependencies, PullService, PullTargetChoice, PullTargetSelectionError,
        PullTargetSelector,
    },
    domain::{
        CredentialKey, CredentialScope, DatabaseName, DomainAlias, MysqlTlsMode, ProfileName,
        TenantLookup,
    },
    infrastructure::{
        config::{
            AppConfig, AppPaths, ClientRuntimeConfig, ConfigRepository, LocalTargetConfig,
            LocalTargetTrust, MysqlClientConfig, MysqlFamily, SourceProfileConfig,
            TenantResolverConfig,
        },
        credentials::{
            CredentialStore, MYSQL_OPTION_FILE_CONTAINER_PATH, MemoryCredentialStore,
            MysqlOptionFile,
        },
        docker::DockerTargetDiscovery,
        mysql::{
            ClientCatalog, DockerDumpWorkflow, DockerLocalTargetAttestor, DockerLocalTenantWriter,
            DockerMysqlDumpExecutor, DockerMysqlRestoreExecutor,
        },
        process::TokioProcessRunner,
    },
};
use secrecy::{ExposeSecret, SecretString, zeroize::Zeroize};
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
#[ignore = "creates a second MySQL container, pulls from mysql-8 into it, verifies cache reuse, and cleans both fixtures"]
async fn pulls_a_real_tenant_then_reuses_cache_without_the_source_credential() {
    let expected =
        std::env::var("REPRODB_TEST_MYSQL_CONTAINER").unwrap_or_else(|_| "mysql-8".to_owned());
    let (context, candidates) = DockerTargetDiscovery::new(TokioProcessRunner)
        .discover()
        .await
        .unwrap();
    let source_candidate = candidates
        .into_iter()
        .find(|candidate| candidate.name.as_str() == expected)
        .expect("the expected local MySQL container was not discovered");
    let source_password = local_container_root_password(&context, &expected);
    let suffix = Uuid::new_v4().simple().to_string();
    let target_name = format!("reprodb-target-{}", &suffix[..12]);
    let target_password = SecretString::from("reprodb-integration-target-only");
    let _target_guard = start_target_container(
        &context,
        &target_name,
        ClientCatalog::resolve("8.4").unwrap().image(),
        &target_password,
    );
    let (_, candidates) = DockerTargetDiscovery::new(TokioProcessRunner)
        .discover()
        .await
        .unwrap();
    let target_candidate = candidates
        .into_iter()
        .find(|candidate| candidate.name.as_str() == target_name)
        .expect("the temporary target MySQL container was not discovered");
    let database = DatabaseName::try_from(format!("salt_reprodb_pull_{suffix}")).unwrap();
    let domain = DomainAlias::try_from(format!("reprodb-pull-{}", &suffix[..16])).unwrap();
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
                central_database: DatabaseName::try_from("salt_central").unwrap(),
                trust: if target_candidate.managed_by_reprodb {
                    LocalTargetTrust::ReprodbManaged
                } else {
                    LocalTargetTrust::UserConfirmed
                },
                tenant_database_prefix: "salt_".to_owned(),
            }),
            profiles: std::collections::BTreeMap::from([(
                profile_name,
                SourceProfileConfig {
                    host: "127.0.0.1".to_owned(),
                    port: 3306,
                    username: "root".to_owned(),
                    credential_key: source_key,
                    mysql_family: MysqlFamily::Mysql,
                    mysql_series: "8.4".to_owned(),
                    production: false,
                    tls_mode: MysqlTlsMode::Disabled,
                    client: MysqlClientConfig {
                        image: client.image().to_owned(),
                    },
                    tenant_resolver: TenantResolverConfig::SaltCentral {
                        central_database: DatabaseName::try_from("salt_central").unwrap(),
                        allow_domain_lookup: true,
                    },
                },
            )]),
            ..AppConfig::default()
        })
        .unwrap();
    let credentials = MemoryCredentialStore::default();
    credentials
        .set(&source_key, source_password.clone())
        .await
        .unwrap();
    credentials
        .set(&target_key, target_password.clone())
        .await
        .unwrap();

    wait_for_mysql(
        &context,
        target_candidate.id.as_str(),
        client.image(),
        &target_password,
    )
    .await;
    run_mysql_query(
        &context,
        target_candidate.id.as_str(),
        client.image(),
        &target_password,
        MysqlTlsMode::Required,
        None,
        "CREATE DATABASE `salt_central` CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci; \
         CREATE TABLE `salt_central`.`tenants` (`id` VARCHAR(255) NOT NULL PRIMARY KEY, `created_at` TIMESTAMP NULL, `updated_at` TIMESTAMP NULL, `data` JSON NULL) ENGINE=InnoDB; \
         CREATE TABLE `salt_central`.`domains` (`id` BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY, `domain` VARCHAR(255) NOT NULL UNIQUE, `tenant_id` VARCHAR(255) NOT NULL, `created_at` TIMESTAMP NULL, `updated_at` TIMESTAMP NULL, CONSTRAINT `domains_tenant_fk` FOREIGN KEY (`tenant_id`) REFERENCES `tenants` (`id`)) ENGINE=InnoDB",
    )
    .expect("the temporary target salt_central fixture must be created");

    let collision = run_mysql_query(
        &context,
        source_candidate.id.as_str(),
        client.image(),
        &source_password,
        MysqlTlsMode::Disabled,
        Some(&DatabaseName::try_from("salt_central").unwrap()),
        &format!(
            "SELECT (SELECT COUNT(*) FROM information_schema.SCHEMATA WHERE BINARY SCHEMA_NAME = BINARY 0x{database}), (SELECT COUNT(*) FROM tenants WHERE BINARY id = BINARY 0x{tenant}), (SELECT COUNT(*) FROM domains WHERE BINARY domain = BINARY 0x{domain})",
            database = hex_utf8(database.as_str()),
            tenant = hex_utf8(database.as_str()),
            domain = hex_utf8(domain.as_str()),
        ),
    )
    .unwrap();
    assert_eq!(collision, b"0\t0\t0\n");

    let fixture = run_mysql_query(
        &context,
        source_candidate.id.as_str(),
        client.image(),
        &source_password,
        MysqlTlsMode::Disabled,
        None,
        &format!(
            "CREATE DATABASE `{database}` CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci; \
             CREATE TABLE `{database}`.`reprodb_pull_items` (`id` BIGINT NOT NULL PRIMARY KEY, `label` VARCHAR(255) NOT NULL) ENGINE=InnoDB; \
             INSERT INTO `{database}`.`reprodb_pull_items` VALUES (1, CONVERT(0x5361676174656320F09FA782 USING utf8mb4)), (2, 'Polymer'); \
             INSERT INTO `salt_central`.`tenants` (`id`, `created_at`, `updated_at`, `data`) VALUES (CONVERT(0x{tenant} USING utf8mb4), CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, JSON_OBJECT('tenancy_db_name', CONVERT(0x{database_hex} USING utf8mb4), 'tenancy_app_color', 'green')); \
             INSERT INTO `salt_central`.`domains` (`domain`, `tenant_id`, `created_at`, `updated_at`) VALUES (CONVERT(0x{domain} USING utf8mb4), CONVERT(0x{tenant} USING utf8mb4), CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)",
            tenant = hex_utf8(database.as_str()),
            database_hex = hex_utf8(database.as_str()),
            domain = hex_utf8(domain.as_str()),
        ),
    );

    let workflow = DockerDumpWorkflow::new(TokioProcessRunner);
    let attestor = DockerLocalTargetAttestor::new(TokioProcessRunner);
    let dump_executor = DockerMysqlDumpExecutor;
    let service = PullService::new(repository);
    let first = match fixture {
        Ok(_) => service
            .pull(
                &credentials,
                PullDumpDependencies {
                    tenant_resolver: &workflow,
                    preflight: &workflow,
                    executor: &dump_executor,
                },
                PullRestoreDependencies {
                    target_attestor: &attestor,
                    executor: DockerMysqlRestoreExecutor,
                    tenant_writer: DockerLocalTenantWriter::new(TokioProcessRunner),
                    target_selector: &DefaultTarget,
                    database_selector: &SourceDatabase,
                },
                TenantLookup::try_from(domain.as_str()).unwrap(),
                false,
            )
            .await
            .map_err(|error| error.to_string()),
        Err(error) => Err(error),
    };
    let source_credential_removed = credentials.delete(&source_key).await;
    let second = if first.is_ok() && source_credential_removed.is_ok() {
        service
            .pull(
                &credentials,
                PullDumpDependencies {
                    tenant_resolver: &workflow,
                    preflight: &workflow,
                    executor: &dump_executor,
                },
                PullRestoreDependencies {
                    target_attestor: &attestor,
                    executor: DockerMysqlRestoreExecutor,
                    tenant_writer: DockerLocalTenantWriter::new(TokioProcessRunner),
                    target_selector: &DefaultTarget,
                    database_selector: &SourceDatabase,
                },
                TenantLookup::try_from(domain.as_str()).unwrap(),
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
            "SELECT id, HEX(label) FROM reprodb_pull_items ORDER BY id",
        ))
    } else {
        None
    };
    let central_verification = if second.is_ok() {
        Some(run_mysql_query(
            &context,
            target_candidate.id.as_str(),
            client.image(),
            &target_password,
            MysqlTlsMode::Required,
            Some(&DatabaseName::try_from("salt_central").unwrap()),
            &format!(
                "SELECT JSON_UNQUOTE(JSON_EXTRACT(data, '$.tenancy_db_name')), JSON_UNQUOTE(JSON_EXTRACT(data, '$.tenancy_app_color')) FROM tenants WHERE BINARY id = BINARY 0x{}",
                hex_utf8(database.as_str())
            ),
        ))
    } else {
        None
    };

    let central_cleanup = run_mysql_query(
        &context,
        source_candidate.id.as_str(),
        client.image(),
        &source_password,
        MysqlTlsMode::Disabled,
        Some(&DatabaseName::try_from("salt_central").unwrap()),
        &format!(
            "DELETE FROM tenants WHERE BINARY id = BINARY 0x{}",
            hex_utf8(database.as_str())
        ),
    );
    let database_cleanup = run_mysql_query(
        &context,
        source_candidate.id.as_str(),
        client.image(),
        &source_password,
        MysqlTlsMode::Disabled,
        None,
        &format!("DROP DATABASE IF EXISTS `{database}`"),
    );

    central_cleanup.expect("the unique pull tenant fixture must be removed");
    database_cleanup.expect("the unique pull database fixture must be removed");
    source_credential_removed.expect("the source credential must be removable before cache hit");
    let first = first.expect("the cache miss must create and restore a real dump");
    let second = second.expect("the cache hit must restore without a source credential");
    assert_eq!(first.cache, PullCacheUse::Created);
    assert!(matches!(second.cache, PullCacheUse::Hit { .. }));
    assert_eq!(first.restored.plan.dump_id, second.restored.plan.dump_id);
    assert_eq!(first.restored.plan.database, database);
    assert_eq!(first.restored.plan.container, target_candidate.name);
    assert_eq!(
        String::from_utf8(verification.unwrap().unwrap()).unwrap(),
        "1\t5361676174656320F09FA782\n2\t506F6C796D6572\n"
    );
    assert_eq!(
        String::from_utf8(central_verification.unwrap().unwrap()).unwrap(),
        format!("{database}\tgreen\n")
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

fn start_target_container(
    context: &str,
    name: &str,
    image: &str,
    password: &SecretString,
) -> TemporaryDockerContainer {
    let status = Command::new("docker")
        .env("MYSQL_ROOT_PASSWORD", password.expose_secret())
        .args([
            "--context",
            context,
            "run",
            "--detach",
            "--rm",
            "--pull=never",
            "--name",
            name,
            "--label",
            "com.sagatech.reprodb.target=true",
            "--env",
            "MYSQL_ROOT_PASSWORD",
            image,
        ])
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

fn hex_utf8(value: &str) -> String {
    value.bytes().map(|byte| format!("{byte:02X}")).collect()
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
        "type=bind,src={},dst={MYSQL_OPTION_FILE_CONTAINER_PATH},readonly",
        option_file.path().display()
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
