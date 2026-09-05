#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::process::{Command, Stdio};

use reprodb::infrastructure::{
    mysql::{ClientCatalog, DockerMysqlClientRuntime},
    process::TokioProcessRunner,
};
use secrecy::{SecretString, zeroize::Zeroize};

#[tokio::test]
#[ignore = "requires the local mysql-8 container and reads its test-only root password"]
async fn docker_client_connects_to_the_local_mysql_source() {
    let context =
        std::env::var("REPRODB_TEST_DOCKER_CONTEXT").unwrap_or_else(|_| "default".to_owned());
    let container =
        std::env::var("REPRODB_TEST_MYSQL_CONTAINER").unwrap_or_else(|_| "mysql-8".to_owned());
    let password = local_container_root_password(&context, &container);
    let approved = ClientCatalog::resolve("8.4").unwrap();
    let runtime = DockerMysqlClientRuntime::new(TokioProcessRunner);
    let prepared = runtime
        .prepare(&context, approved.series(), approved.image())
        .await
        .unwrap();
    let option_file = runtime
        .create_option_file("127.0.0.1", 3306, "root", &password)
        .unwrap();

    let server = runtime
        .probe_connection(&prepared, &option_file)
        .await
        .unwrap();

    assert_eq!(server.version.major, 8);
    assert!(server.vendor.contains("MySQL"));
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
