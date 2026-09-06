#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::process::{Command, Stdio};

use reprodb::{
    application::LocalTargetGate,
    domain::{CredentialKey, CredentialScope, DatabaseName},
    infrastructure::{
        config::{
            AppConfig, AppPaths, ClientRuntimeConfig, ConfigRepository,
            DEFAULT_TENANT_DATABASE_PREFIX, LocalTargetConfig, LocalTargetTrust,
        },
        credentials::{CredentialStore, MemoryCredentialStore},
        docker::DockerTargetDiscovery,
        mysql::DockerLocalTargetAttestor,
        process::TokioProcessRunner,
    },
};
use secrecy::{SecretString, zeroize::Zeroize};
use tempfile::TempDir;

#[tokio::test]
#[ignore = "requires Docker and the local mysql-8 container"]
async fn attests_the_exact_local_container_before_authorizing_a_tenant_database() {
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
    let key = CredentialKey::new(CredentialScope::Target);
    let temp = TempDir::new().unwrap();
    let repository = ConfigRepository::new(AppPaths::new(
        temp.path().join("config"),
        temp.path().join("cache"),
        temp.path().join("data"),
    ));
    repository
        .save(&AppConfig {
            client_runtime: ClientRuntimeConfig {
                docker_context: Some(context.clone()),
                ..ClientRuntimeConfig::default()
            },
            local_target: Some(LocalTargetConfig {
                docker_context: context,
                container_name: candidate.name,
                container_id: candidate.id,
                username: "root".to_owned(),
                credential_key: key,
                central_database: DatabaseName::try_from("salt_central").unwrap(),
                trust: if candidate.managed_by_reprodb {
                    LocalTargetTrust::ReprodbManaged
                } else {
                    LocalTargetTrust::UserConfirmed
                },
                tenant_database_prefix: DEFAULT_TENANT_DATABASE_PREFIX.to_owned(),
            }),
            ..AppConfig::default()
        })
        .unwrap();
    let credentials = MemoryCredentialStore::default();
    credentials
        .set(&key, local_container_root_password(&expected))
        .await
        .unwrap();

    let guarded = LocalTargetGate::new(repository)
        .verify(
            &credentials,
            &DockerLocalTargetAttestor::new(TokioProcessRunner),
        )
        .await
        .unwrap();
    let authorized = guarded
        .authorize_tenant_database(DatabaseName::try_from("salt_polymer").unwrap())
        .unwrap();

    assert_eq!(authorized.container_name().as_str(), expected);
    assert_eq!(authorized.database().as_str(), "salt_polymer");
    assert_eq!(authorized.server_version().to_string(), "8.4.4");
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
