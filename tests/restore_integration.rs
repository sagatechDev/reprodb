#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::{
    io::Cursor,
    process::{Command, Stdio},
    sync::Arc,
};

use reprodb::{
    application::{LocalTargetGate, RestoreService},
    domain::{
        CredentialKey, CredentialScope, DatabaseEncoding, DatabaseName, DumpArtifactCompletion,
        DumpArtifactContext, MysqlTlsMode, MysqlVersion, ProfileName, Sha256Digest,
    },
    infrastructure::{
        artifact_store::LocalArtifactStore,
        compression::{NoCompressionProgress, ZstdCompressor},
        config::{
            AppConfig, AppPaths, ClientRuntimeConfig, ConfigRepository, LocalTargetConfig,
            LocalTargetTrust,
        },
        credentials::{
            CredentialStore, MYSQL_OPTION_FILE_CONTAINER_PATH, MemoryCredentialStore,
            MysqlOptionFile,
        },
        docker::DockerTargetDiscovery,
        mysql::{ClientCatalog, DockerLocalTargetAttestor, DockerMysqlRestoreExecutor},
        process::TokioProcessRunner,
    },
};
use secrecy::{SecretString, zeroize::Zeroize};
use tempfile::TempDir;
use uuid::Uuid;

#[tokio::test]
#[ignore = "creates, restores, verifies and removes a unique database in the local mysql-8 container"]
async fn restores_a_validated_zstd_artifact_into_the_guarded_mysql_8_target() {
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
    let database =
        DatabaseName::try_from(format!("reprodb_restore_{}", Uuid::new_v4().simple())).unwrap();
    let temp = TempDir::new().unwrap();
    let paths = AppPaths::new(
        temp.path().join("config"),
        temp.path().join("cache"),
        temp.path().join("data"),
    );
    let repository = ConfigRepository::new(paths.clone());
    let key = CredentialKey::new(CredentialScope::Target);
    repository
        .save(&AppConfig {
            client_runtime: ClientRuntimeConfig {
                docker_context: Some(context.clone()),
                ..ClientRuntimeConfig::default()
            },
            local_target: Some(LocalTargetConfig {
                docker_context: context.clone(),
                container_name: candidate.name,
                container_id: candidate.id,
                username: "root".to_owned(),
                credential_key: key,
                trust: if candidate.managed_by_reprodb {
                    LocalTargetTrust::ReprodbManaged
                } else {
                    LocalTargetTrust::UserConfirmed
                },
            }),
            ..AppConfig::default()
        })
        .unwrap();
    let credentials = MemoryCredentialStore::default();
    credentials.set(&key, password.clone()).await.unwrap();
    let guarded = LocalTargetGate::new(repository.clone())
        .verify(
            &credentials,
            &DockerLocalTargetAttestor::new(TokioProcessRunner),
        )
        .await
        .unwrap();
    let authorized = guarded.authorize_database(database.clone()).unwrap();

    let profile = ProfileName::try_from("local-source").unwrap();
    let client = ClientCatalog::resolve("8.4").unwrap();
    let store = LocalArtifactStore::new(paths.cache_dir());
    let stage = store.begin(&profile, &database).unwrap();
    let dump_id = stage.dump_id();
    let output = stage.create_dump_writer().unwrap();
    let sql = concat!(
        "CREATE TABLE `reprodb_items` (",
        "`id` BIGINT NOT NULL PRIMARY KEY,",
        "`label` VARCHAR(255) CHARACTER SET utf8mb4 NOT NULL,",
        "`payload` BLOB NULL) ENGINE=InnoDB;\n",
        "INSERT INTO `reprodb_items` VALUES ",
        "(1,'Acme 🧪',X'00FF'),(2,'Globex',NULL);\n"
    );
    let metrics = ZstdCompressor::default()
        .compress(
            Cursor::new(sql.as_bytes()),
            output,
            Arc::new(NoCompressionProgress),
        )
        .await
        .unwrap();
    let metadata = reprodb::domain::DumpArtifactMetadata::try_new(
        dump_id,
        DumpArtifactContext {
            database: database.clone(),
            profile: profile.clone(),
            source_fingerprint: Sha256Digest::from_bytes([1; 32]),
            source_server_uuid: "11111111-1111-4111-8111-111111111111".parse().unwrap(),
            source_version: "8.4.4".parse::<MysqlVersion>().unwrap(),
            client_version: client.version(),
            database_encoding: DatabaseEncoding::try_new(
                "utf8mb4".to_owned(),
                "utf8mb4_0900_ai_ci".to_owned(),
            )
            .unwrap(),
            policy_version: 1,
        },
        DumpArtifactCompletion {
            created_at_unix_seconds: 100,
            completed_at_unix_seconds: 101,
            uncompressed_bytes: metrics.input_bytes(),
            compressed_bytes: metrics.compressed_bytes(),
            sql_sha256: metrics.input_sha256(),
            artifact_sha256: metrics.compressed_sha256(),
        },
    )
    .unwrap();
    stage.publish(&metadata, &metrics).unwrap();

    let attestor = DockerLocalTargetAttestor::new(TokioProcessRunner);
    let service = RestoreService::new(repository);
    let restore_result = service
        .restore(
            &credentials,
            &attestor,
            DockerMysqlRestoreExecutor::default(),
            database.clone(),
            dump_id,
        )
        .await;
    let post_restore = match restore_result {
        Ok(_) => Some(
            service
                .restore(
                    &credentials,
                    &attestor,
                    DockerMysqlRestoreExecutor::default(),
                    database.clone(),
                    dump_id,
                )
                .await
                .map_err(|error| error.to_string()),
        ),
        Err(_) => None,
    };
    let data_verification = if post_restore.as_ref().is_some_and(Result::is_ok) {
        Some(run_mysql_query(
            &context,
            authorized.container_id().as_str(),
            client.image(),
            authorized.username(),
            &password,
            Some(&database),
            "SELECT id, HEX(label), HEX(payload) FROM reprodb_items ORDER BY id",
        ))
    } else {
        None
    };
    let database_cleanup = run_mysql_query(
        &context,
        authorized.container_id().as_str(),
        client.image(),
        authorized.username(),
        &password,
        None,
        &format!("DROP DATABASE IF EXISTS `{database}`"),
    );

    database_cleanup.expect("the unique restore fixture database must be removed");
    let completed = post_restore
        .expect("the managed artifact must restore successfully")
        .expect("the public restore workflow must be safely repeatable");
    assert_eq!(completed.plan.database, database);
    assert_eq!(completed.imported_bytes, sql.len() as u64);
    assert_eq!(
        String::from_utf8(data_verification.unwrap().unwrap()).unwrap(),
        format!(
            "1\t{}\t00FF\n2\t{}\tNULL\n",
            hex_utf8("Acme 🧪"),
            hex_utf8("Globex")
        )
    );
}

fn hex_utf8(value: &str) -> String {
    value.bytes().map(|byte| format!("{byte:02X}")).collect()
}

fn run_mysql_query(
    context: &str,
    container_id: &str,
    client_image: &str,
    username: &str,
    password: &SecretString,
    database: Option<&DatabaseName>,
    query: &str,
) -> Result<Vec<u8>, String> {
    let option_file = MysqlOptionFile::create(
        "127.0.0.1",
        3306,
        username,
        password,
        MysqlTlsMode::Disabled,
    )
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
