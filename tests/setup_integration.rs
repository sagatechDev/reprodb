#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::process::{Command, Stdio};

use reprodb::{
    application::{NewLocalTargetInput, SetupService},
    domain::DatabaseName,
    infrastructure::{
        config::{AppPaths, ConfigRepository},
        credentials::{CredentialStore, MemoryCredentialStore},
        docker::DockerTargetDiscovery,
        mysql::DockerLocalTargetVerifier,
        process::TokioProcessRunner,
    },
};
use secrecy::{SecretString, zeroize::Zeroize};
use tempfile::TempDir;

#[tokio::test]
#[ignore = "requires Docker and the local mysql-8 container"]
async fn configures_the_real_local_mysql_target() {
    let expected =
        std::env::var("REPRODB_TEST_MYSQL_CONTAINER").unwrap_or_else(|_| "mysql-8".to_owned());
    let discovery = DockerTargetDiscovery::new(TokioProcessRunner);
    let (context, candidates) = discovery.discover().await.unwrap();
    let candidate = candidates
        .into_iter()
        .find(|candidate| candidate.name.as_str() == expected)
        .expect("the expected local MySQL container was not discovered");
    let password = local_container_root_password(&context, candidate.name.as_str());
    let temp = TempDir::new().unwrap();
    let repository = ConfigRepository::new(AppPaths::new(
        temp.path().join("config"),
        temp.path().join("cache"),
        temp.path().join("data"),
    ));
    let credentials = MemoryCredentialStore::default();
    let verifier = DockerLocalTargetVerifier::new(TokioProcessRunner);
    let input = NewLocalTargetInput {
        docker_context: context,
        container_name: candidate.name,
        container_id: candidate.id,
        username: "root".to_owned(),
        password,
        central_database: DatabaseName::try_from("salt_central").unwrap(),
        tenant_database_prefix: "salt_".to_owned(),
        managed_by_reprodb: candidate.managed_by_reprodb,
    };

    let configured = SetupService::new(repository.clone())
        .configure(&credentials, &verifier, input)
        .await
        .unwrap();

    assert_eq!(configured.container_name.as_str(), expected);
    assert_eq!(configured.server_version.to_string(), "8.4.4");
    let config = repository.load().unwrap();
    let target = config.local_target.unwrap();
    assert_eq!(target.container_name.as_str(), expected);
    assert!(credentials.get(&target.credential_key).await.is_ok());
    assert!(
        !std::fs::read_to_string(repository.paths().config_file())
            .unwrap()
            .contains("password")
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
