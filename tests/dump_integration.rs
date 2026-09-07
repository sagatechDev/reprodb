#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::{
    collections::BTreeMap,
    process::{Command, Stdio},
};

use reprodb::{
    application::DumpService,
    domain::{
        CredentialKey, CredentialScope, DatabaseName, MysqlTlsMode, ProfileName, TenantLookup,
    },
    infrastructure::{
        config::{
            AppConfig, AppPaths, ClientRuntimeConfig, ClientRuntimeKind, ConfigRepository,
            MysqlClientConfig, MysqlFamily, SourceProfileConfig, TenantResolverConfig,
        },
        credentials::{CredentialStore, MemoryCredentialStore},
        mysql::{ClientCatalog, DockerDumpWorkflow, DockerMysqlDumpExecutor},
        process::TokioProcessRunner,
    },
};
use secrecy::{SecretString, zeroize::Zeroize};
use tempfile::TempDir;

#[tokio::test]
#[ignore = "requires the local mysql-8 container with salt_sagatec and the approved client image"]
async fn dumps_the_real_local_salt_tenant_into_a_managed_zstd_artifact() {
    let container =
        std::env::var("REPRODB_TEST_MYSQL_CONTAINER").unwrap_or_else(|_| "mysql-8".to_owned());
    let docker_context = current_docker_context();
    let password = local_container_root_password(&container);
    let temp = TempDir::new().unwrap();
    let repository = ConfigRepository::new(AppPaths::new(
        temp.path().join("config"),
        temp.path().join("cache"),
        temp.path().join("data"),
    ));
    let credential_key = CredentialKey::new(CredentialScope::Source);
    let profile_name = ProfileName::try_from("local-source").unwrap();
    let client = ClientCatalog::resolve("8.4").unwrap();
    let mut profiles = BTreeMap::new();
    profiles.insert(
        profile_name.clone(),
        SourceProfileConfig {
            host: "127.0.0.1".to_owned(),
            port: 3306,
            username: "root".to_owned(),
            credential_key,
            mysql_family: MysqlFamily::Mysql,
            mysql_series: "8.4".to_owned(),
            production: false,
            tls_mode: MysqlTlsMode::Required,
            client: MysqlClientConfig {
                image: client.image().to_owned(),
            },
            tenant_resolver: TenantResolverConfig::SaltCentral {
                central_database: DatabaseName::try_from("salt_central").unwrap(),
                allow_domain_lookup: true,
            },
        },
    );
    repository
        .save(&AppConfig {
            active_profile: Some(profile_name),
            client_runtime: ClientRuntimeConfig {
                kind: ClientRuntimeKind::Docker,
                docker_context: Some(docker_context),
            },
            profiles,
            ..AppConfig::default()
        })
        .unwrap();
    let credentials = MemoryCredentialStore::default();
    credentials.set(&credential_key, password).await.unwrap();
    let workflow = DockerDumpWorkflow::new(TokioProcessRunner);

    let created = DumpService::new(repository)
        .create(
            &credentials,
            &workflow,
            &workflow,
            &DockerMysqlDumpExecutor::default(),
            TenantLookup::try_from("sagatec").unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(created.tenant_id.as_str(), "salt_sagatec");
    assert_eq!(created.database.as_str(), "salt_sagatec");
    assert!(created.uncompressed_bytes > created.compressed_bytes);
    let sql = zstd::stream::decode_all(
        std::fs::File::open(created.artifact_path.join("dump.sql.zst")).unwrap(),
    )
    .unwrap();
    assert!(sql.starts_with(b"-- MySQL dump"));
    assert!(
        sql.windows(b"CREATE TABLE".len())
            .any(|part| part == b"CREATE TABLE")
    );
}

fn current_docker_context() -> String {
    let output = Command::new("docker")
        .args(["context", "show"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .expect("Docker must be available for this ignored integration test");
    assert!(output.status.success(), "could not discover Docker context");
    String::from_utf8(output.stdout)
        .expect("Docker context must be valid UTF-8")
        .trim()
        .to_owned()
}

fn local_container_root_password(container: &str) -> SecretString {
    let output = Command::new("docker")
        .args([
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
