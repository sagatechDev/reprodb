#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::process::{Command, Stdio};

use reprodb::{
    application::{PullCacheUse, PullDumpDependencies, PullRestoreDependencies, PullService},
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
use secrecy::{SecretString, zeroize::Zeroize};
use tempfile::TempDir;
use uuid::Uuid;

#[tokio::test]
#[ignore = "creates, pulls twice and removes a unique Salt tenant in the local mysql-8 container"]
async fn pulls_a_real_tenant_then_reuses_cache_without_the_source_credential() {
    let expected =
        std::env::var("REPRODB_TEST_MYSQL_CONTAINER").unwrap_or_else(|_| "mysql-8".to_owned());
    let (context, candidates) = DockerTargetDiscovery::new(TokioProcessRunner)
        .discover()
        .await
        .unwrap();
    let candidate = candidates
        .into_iter()
        .find(|candidate| candidate.name.as_str() == expected)
        .expect("the expected local MySQL container was not discovered");
    let password = local_container_root_password(&context, &expected);
    let suffix = Uuid::new_v4().simple().to_string();
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
                container_name: candidate.name.clone(),
                container_id: candidate.id.clone(),
                username: "root".to_owned(),
                credential_key: target_key,
                central_database: DatabaseName::try_from("salt_central").unwrap(),
                trust: if candidate.managed_by_reprodb {
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
        .set(&source_key, password.clone())
        .await
        .unwrap();
    credentials
        .set(&target_key, password.clone())
        .await
        .unwrap();

    let collision = run_mysql_query(
        &context,
        candidate.id.as_str(),
        client.image(),
        &password,
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
        candidate.id.as_str(),
        client.image(),
        &password,
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
            candidate.id.as_str(),
            client.image(),
            &password,
            Some(&database),
            "SELECT id, HEX(label) FROM reprodb_pull_items ORDER BY id",
        ))
    } else {
        None
    };
    let central_verification = if second.is_ok() {
        Some(run_mysql_query(
            &context,
            candidate.id.as_str(),
            client.image(),
            &password,
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
        candidate.id.as_str(),
        client.image(),
        &password,
        Some(&DatabaseName::try_from("salt_central").unwrap()),
        &format!(
            "DELETE FROM tenants WHERE BINARY id = BINARY 0x{}",
            hex_utf8(database.as_str())
        ),
    );
    let database_cleanup = run_mysql_query(
        &context,
        candidate.id.as_str(),
        client.image(),
        &password,
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
    assert_eq!(
        String::from_utf8(verification.unwrap().unwrap()).unwrap(),
        "1\t5361676174656320F09FA782\n2\t506F6C796D6572\n"
    );
    assert_eq!(
        String::from_utf8(central_verification.unwrap().unwrap()).unwrap(),
        format!("{database}\tgreen\n")
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
    database: Option<&DatabaseName>,
    query: &str,
) -> Result<Vec<u8>, String> {
    let option_file =
        MysqlOptionFile::create("127.0.0.1", 3306, "root", password, MysqlTlsMode::Disabled)
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
