#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::process::{Command, Stdio};

use reprodb::{
    application::{NewProfileInput, ProfileService},
    domain::{MysqlTlsMode, ProfileName},
    infrastructure::{
        config::{AppPaths, ConfigRepository},
        credentials::{CredentialStore, MemoryCredentialStore},
        mysql::DockerSourceProfileVerifier,
        process::TokioProcessRunner,
    },
};
use secrecy::{SecretString, zeroize::Zeroize};
use tempfile::TempDir;

#[tokio::test]
#[ignore = "requires the local mysql-8 container and reads its test-only root password"]
async fn adds_a_profile_after_verifying_the_real_local_mysql_source() {
    let container =
        std::env::var("REPRODB_TEST_MYSQL_CONTAINER").unwrap_or_else(|_| "mysql-8".to_owned());
    let password = local_container_root_password(&container);
    let temp = TempDir::new().unwrap();
    let repository = ConfigRepository::new(AppPaths::new(
        temp.path().join("config"),
        temp.path().join("cache"),
        temp.path().join("data"),
    ));
    let credentials = MemoryCredentialStore::default();
    let verifier = DockerSourceProfileVerifier::new(TokioProcessRunner);
    let input = NewProfileInput {
        name: ProfileName::try_from("local-mysql").unwrap(),
        host: "127.0.0.1".to_owned(),
        port: 3306,
        username: "root".to_owned(),
        password,
        tls_mode: MysqlTlsMode::Required,
        production: false,
    };

    let created = ProfileService::new(repository.clone())
        .add(&credentials, &verifier, input)
        .await
        .unwrap();

    assert_eq!(created.server_version.to_string(), "8.4.4");
    assert_eq!(created.client_version.to_string(), "8.4.4");
    let config = repository.load().unwrap();
    let profile = config.profiles.get(&created.name).unwrap();
    assert!(credentials.get(&profile.credential_key).await.is_ok());
    assert!(
        !std::fs::read_to_string(repository.paths().config_file())
            .unwrap()
            .contains("password")
    );
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
