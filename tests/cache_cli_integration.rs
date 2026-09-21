use std::{collections::BTreeMap, io::Cursor, sync::Arc};

use assert_cmd::Command;
use predicates::prelude::*;
use reprodb::{
    domain::{
        CredentialKey, CredentialScope, DatabaseEncoding, DatabaseName, DumpArtifactCompletion,
        DumpArtifactContext, DumpArtifactMetadata, MYSQL_8_DUMP_POLICY_VERSION, MysqlTlsMode,
        MysqlVersion, ProfileName,
    },
    infrastructure::{
        artifact_store::LocalArtifactStore,
        compression::{NoCompressionProgress, ZstdCompressor},
        config::{
            AppConfig, AppPaths, ClientRuntimeConfig, ConfigRepository, MysqlClientConfig,
            MysqlFamily, SourceProfileConfig, source_profile_fingerprint,
        },
        mysql::ClientCatalog,
    },
};

#[tokio::test]
async fn cli_lists_and_prunes_a_managed_dump_without_accessing_docker_or_credentials() {
    let home = tempfile::tempdir().unwrap();
    let paths = AppPaths::from_root(home.path());
    let repository = ConfigRepository::new(paths.clone());
    let profile_name = ProfileName::try_from("local-source").unwrap();
    let client = ClientCatalog::resolve("8.4").unwrap();
    let profile = SourceProfileConfig {
        host: "127.0.0.1".to_owned(),
        port: 3306,
        username: "root".to_owned(),
        credential_key: CredentialKey::new(CredentialScope::Source),
        mysql_family: MysqlFamily::Mysql,
        mysql_series: "8.4".to_owned(),
        production: false,
        tls_mode: MysqlTlsMode::Disabled,
        tls_material: Default::default(),
        client: MysqlClientConfig {
            image: client.image().to_owned(),
        },
    };
    let fingerprint = source_profile_fingerprint(&profile_name, &profile);
    repository
        .save(&AppConfig {
            active_profile: Some(profile_name.clone()),
            client_runtime: ClientRuntimeConfig::default(),
            profiles: BTreeMap::from([(profile_name.clone(), profile)]),
            ..AppConfig::default()
        })
        .unwrap();

    let database = DatabaseName::try_from("acme_production").unwrap();
    let stage = LocalArtifactStore::new(paths.cache_dir())
        .begin(&profile_name, &database)
        .unwrap();
    let dump_id = stage.dump_id();
    let metrics = ZstdCompressor::default()
        .compress(
            Cursor::new(b"CREATE TABLE `items` (`id` BIGINT);\n"),
            stage.create_dump_writer().unwrap(),
            Arc::new(NoCompressionProgress),
        )
        .await
        .unwrap();
    let completed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let metadata = DumpArtifactMetadata::try_new(
        dump_id,
        DumpArtifactContext {
            database: DatabaseName::try_from("acme_production").unwrap(),
            profile: profile_name,
            source_fingerprint: fingerprint,
            source_server_uuid: "11111111-1111-4111-8111-111111111111".parse().unwrap(),
            source_version: "8.4.4".parse::<MysqlVersion>().unwrap(),
            client_version: client.version(),
            database_encoding: DatabaseEncoding::try_new(
                "utf8mb4".to_owned(),
                "utf8mb4_0900_ai_ci".to_owned(),
            )
            .unwrap(),
            policy_version: MYSQL_8_DUMP_POLICY_VERSION,
        },
        DumpArtifactCompletion {
            created_at_unix_seconds: completed_at,
            completed_at_unix_seconds: completed_at,
            uncompressed_bytes: metrics.input_bytes(),
            compressed_bytes: metrics.compressed_bytes(),
            sql_sha256: metrics.input_sha256(),
            artifact_sha256: metrics.compressed_sha256(),
        },
    )
    .unwrap();
    let artifact = stage.publish(&metadata, &metrics).unwrap();

    Command::cargo_bin("reprodb")
        .unwrap()
        .env("REPRODB_HOME", home.path())
        .args(["cache", "list", "--color", "never"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("✓ fresh  acme_production")
                .and(predicate::str::contains("Profile: local-source"))
                .and(predicate::str::contains(dump_id.to_string())),
        );

    Command::cargo_bin("reprodb")
        .unwrap()
        .env("REPRODB_HOME", home.path())
        .args([
            "cache",
            "prune",
            "acme_production",
            "--all",
            "--yes",
            "--color",
            "never",
        ])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("Scope:     acme_production")
                .and(predicate::str::contains("Criterion: every dump"))
                .and(predicate::str::contains("1 managed dump removed")),
        );
    assert!(!artifact.path().exists());

    Command::cargo_bin("reprodb")
        .unwrap()
        .env("REPRODB_HOME", home.path())
        .args(["cache", "list", "--color", "never"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No managed dumps found"));
}
