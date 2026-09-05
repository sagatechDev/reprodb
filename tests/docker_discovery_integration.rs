#![cfg(any(target_os = "macos", target_os = "linux"))]

use reprodb::infrastructure::{
    docker::{ContainerState, DockerTargetDiscovery},
    process::TokioProcessRunner,
};

#[tokio::test]
#[ignore = "requires Docker and the local mysql-8 container"]
async fn discovers_the_real_local_mysql_target() {
    let expected =
        std::env::var("REPRODB_TEST_MYSQL_CONTAINER").unwrap_or_else(|_| "mysql-8".to_owned());
    let discovery = DockerTargetDiscovery::new(TokioProcessRunner);

    let (context, candidates) = discovery.discover().await.unwrap();

    assert!(!context.is_empty());
    let target = candidates
        .iter()
        .find(|candidate| candidate.name.as_str() == expected)
        .expect("the expected local MySQL container was not discovered");
    assert_eq!(target.state, ContainerState::Running);
    assert!(target.exposes_mysql_port);
    assert_eq!(target.image, "mysql:8");
}
