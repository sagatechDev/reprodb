#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::process::{Command, Stdio};

use reprodb::{
    domain::{DatabaseName, MysqlTlsMode, TenantLookup, TenantMatch, TenantResolver},
    infrastructure::{
        docker::DockerTargetDiscovery,
        mysql::{ClientCatalog, DockerSaltCentralTenantResolver, SaltCentralSource},
        process::TokioProcessRunner,
    },
};
use secrecy::{SecretString, zeroize::Zeroize};

#[tokio::test]
#[ignore = "requires Docker and the observed local mysql-8 salt_central fixture"]
async fn resolves_observed_salt_domains_from_the_real_central_database() {
    let expected =
        std::env::var("REPRODB_TEST_MYSQL_CONTAINER").unwrap_or_else(|_| "mysql-8".to_owned());
    let discovery = DockerTargetDiscovery::new(TokioProcessRunner);
    let (context, candidates) = discovery.discover().await.unwrap();
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate.name.as_str() == expected),
        "the expected local MySQL container was not discovered"
    );
    let resolver = DockerSaltCentralTenantResolver::new(
        TokioProcessRunner,
        SaltCentralSource {
            docker_context: context.clone(),
            host: "127.0.0.1".to_owned(),
            port: 3306,
            username: "root".to_owned(),
            password: local_container_root_password(&context, &expected),
            tls_mode: MysqlTlsMode::Required,
            tls_material: Default::default(),
            central_database: DatabaseName::try_from("salt_central").unwrap(),
            client: ClientCatalog::resolve("8.4").unwrap(),
        },
        true,
    );

    for (lookup, tenant_id, database) in [
        ("sagatec", "salt_sagatec", "salt_sagatec"),
        ("polymer", "salt_polymer", "salt_polymer"),
    ] {
        let resolved = resolver
            .resolve(&TenantLookup::try_from(lookup).unwrap())
            .await
            .unwrap();

        assert_eq!(resolved.tenant_id.as_str(), tenant_id);
        assert_eq!(resolved.database.as_str(), database);
        assert_eq!(resolved.matched_by, TenantMatch::Domain);
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
