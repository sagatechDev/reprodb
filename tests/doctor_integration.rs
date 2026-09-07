#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::process::{Command, Stdio};

use reprodb::{
    application::{
        DoctorService, NewLocalTargetInput, NewProfileInput, ProfileService, SetupService,
    },
    domain::{DatabaseName, MysqlTlsMode, ProfileName},
    infrastructure::{
        config::{AppPaths, ConfigRepository},
        credentials::MemoryCredentialStore,
        docker::{DockerCliDoctorInspector, DockerTargetDiscovery},
        filesystem::LocalFilesystemInspector,
        mysql::{DockerLocalTargetVerifier, DockerSourceProfileVerifier},
        process::TokioProcessRunner,
    },
};
use secrecy::{SecretString, zeroize::Zeroize};
use tempfile::TempDir;

#[tokio::test]
#[ignore = "requires Docker and the local mysql-8 container"]
async fn doctor_validates_the_real_environment_without_changing_configuration() {
    let expected =
        std::env::var("REPRODB_TEST_MYSQL_CONTAINER").unwrap_or_else(|_| "mysql-8".to_owned());
    let discovery = DockerTargetDiscovery::new(TokioProcessRunner);
    let (context, candidates) = discovery.discover().await.unwrap();
    let candidate = candidates
        .into_iter()
        .find(|candidate| candidate.name.as_str() == expected)
        .expect("the expected local MySQL container was not discovered");
    let temp = TempDir::new().unwrap();
    let repository = ConfigRepository::new(AppPaths::new(
        temp.path().join("config"),
        temp.path().join("cache"),
        temp.path().join("data"),
    ));
    let credentials = MemoryCredentialStore::default();

    ProfileService::new(repository.clone())
        .add(
            &credentials,
            &DockerSourceProfileVerifier::new(TokioProcessRunner),
            NewProfileInput {
                name: ProfileName::try_from("local-mysql").unwrap(),
                host: "127.0.0.1".to_owned(),
                port: 3306,
                username: "root".to_owned(),
                password: local_container_root_password(&context, &expected),
                tls_mode: MysqlTlsMode::Required,
                tls_material: Default::default(),
                production: false,
            },
        )
        .await
        .unwrap();

    SetupService::new(repository.clone())
        .configure(
            &credentials,
            &DockerLocalTargetVerifier::new(TokioProcessRunner),
            NewLocalTargetInput {
                docker_context: context,
                container_name: candidate.name,
                container_id: candidate.id,
                username: "root".to_owned(),
                password: local_container_root_password_from_current_context(&expected),
                central_database: DatabaseName::try_from("salt_central").unwrap(),
                tenant_database_prefix: "salt_".to_owned(),
                managed_by_reprodb: candidate.managed_by_reprodb,
            },
        )
        .await
        .unwrap();

    let config_before = std::fs::read(repository.paths().config_file()).unwrap();
    let report = DoctorService::new(repository.clone())
        .run(
            &credentials,
            &DockerCliDoctorInspector::new(TokioProcessRunner),
            &LocalFilesystemInspector,
            &DockerSourceProfileVerifier::read_only(TokioProcessRunner),
            &DockerLocalTargetVerifier::read_only(TokioProcessRunner),
        )
        .await;

    assert!(report.is_ready(), "doctor report was not ready: {report:?}");
    assert_eq!(
        std::fs::read(repository.paths().config_file()).unwrap(),
        config_before
    );
    let connection_details = report
        .checks
        .iter()
        .filter(|check| check.label.ends_with("connection"))
        .map(|check| check.detail.as_str())
        .collect::<Vec<_>>();
    assert_eq!(connection_details.len(), 2);
    assert!(connection_details.iter().all(|detail| {
        detail.contains("server") && detail.contains("client 8.4.4") && detail.contains("TLS_")
    }));
}

fn local_container_root_password(context: &str, container: &str) -> SecretString {
    inspect_container_password(Some(context), container)
}

fn local_container_root_password_from_current_context(container: &str) -> SecretString {
    inspect_container_password(None, container)
}

fn inspect_container_password(context: Option<&str>, container: &str) -> SecretString {
    let mut command = Command::new("docker");
    if let Some(context) = context {
        command.args(["--context", context]);
    }
    let output = command
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

    let mut environment =
        String::from_utf8(output.stdout).expect("test container environment must be valid UTF-8");
    let password = environment
        .lines()
        .find_map(|line| line.strip_prefix("MYSQL_ROOT_PASSWORD="))
        .map(str::to_owned)
        .expect("test container does not expose MYSQL_ROOT_PASSWORD");
    environment.zeroize();

    SecretString::from(password)
}
