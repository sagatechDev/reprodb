use async_trait::async_trait;
use secrecy::SecretString;

use crate::{
    domain::{
        AppColor, DatabaseName, LocalTenantFeatures, MysqlTlsMaterialPaths, MysqlTlsMode,
        ResolvedTenant, TenantId, TenantLookup, TenantMatch, TenantResolutionError, TenantResolver,
    },
    infrastructure::{
        mysql::{ApprovedMysqlClient, DockerClientError, DockerMysqlClientRuntime},
        process::ProcessRunner,
    },
};

const MAX_QUERY_OUTPUT_BYTES: usize = 2048;

pub struct SaltCentralSource {
    pub docker_context: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: SecretString,
    pub tls_mode: MysqlTlsMode,
    pub tls_material: MysqlTlsMaterialPaths,
    pub central_database: DatabaseName,
    pub client: ApprovedMysqlClient,
}

pub struct DockerSaltCentralTenantResolver<R> {
    runtime: DockerMysqlClientRuntime<R>,
    source: SaltCentralSource,
    allow_domain_lookup: bool,
}

impl<R> DockerSaltCentralTenantResolver<R>
where
    R: ProcessRunner,
{
    pub fn new(runner: R, source: SaltCentralSource, allow_domain_lookup: bool) -> Self {
        Self {
            runtime: DockerMysqlClientRuntime::new(runner),
            source,
            allow_domain_lookup,
        }
    }
}

#[async_trait]
impl<R> TenantResolver for DockerSaltCentralTenantResolver<R>
where
    R: ProcessRunner,
{
    async fn resolve(
        &self,
        lookup: &TenantLookup,
    ) -> Result<ResolvedTenant, TenantResolutionError> {
        let prepared = self
            .runtime
            .prepare_existing(
                &self.source.docker_context,
                self.source.client.series(),
                self.source.client.image(),
            )
            .await
            .map_err(map_client_error)?;
        let option_file = self
            .runtime
            .create_option_file_with_tls_material(
                &self.source.host,
                self.source.port,
                &self.source.username,
                &self.source.password,
                self.source.tls_mode,
                &self.source.tls_material,
            )
            .map_err(map_client_error)?;
        let query = resolution_query(
            lookup,
            self.allow_domain_lookup,
            &self.source.host,
            self.source.port,
            &self.source.username,
        );
        let output = self
            .runtime
            .query_connection(
                &prepared,
                &option_file,
                &self.source.central_database,
                &query,
            )
            .await
            .map_err(map_client_error)?;

        parse_resolution(&output)
    }
}

fn resolution_query(
    lookup: &TenantLookup,
    allow_domain_lookup: bool,
    source_host: &str,
    source_port: u16,
    source_username: &str,
) -> String {
    let lookup = encode_hex(lookup.as_str().as_bytes());
    let source_host = encode_hex(source_host.as_bytes());
    let source_port = encode_hex(source_port.to_string().as_bytes());
    let source_username = encode_hex(source_username.as_bytes());
    let domain_enabled = u8::from(allow_domain_lookup);
    format!(
        "SET @reprodb_lookup = CONVERT(0x{lookup} USING utf8mb4);\
         SET @reprodb_source_host = CONVERT(0x{source_host} USING utf8mb4);\
         SET @reprodb_source_port = CONVERT(0x{source_port} USING utf8mb4);\
         SET @reprodb_source_username = CONVERT(0x{source_username} USING utf8mb4);\
         SELECT HEX(t.id),\
         CASE WHEN t.data IS NULL THEN 0 \
         WHEN JSON_VALID(t.data) THEN \
           CASE WHEN JSON_EXTRACT(t.data, '$.tenancy_db_name') IS NULL THEN 0 \
                WHEN JSON_TYPE(JSON_EXTRACT(t.data, '$.tenancy_db_name')) = 'STRING' THEN 1 \
                ELSE 2 END \
         ELSE 3 END,\
         CASE WHEN t.data IS NULL THEN '' \
         WHEN JSON_VALID(t.data) THEN \
           CASE WHEN JSON_TYPE(JSON_EXTRACT(t.data, '$.tenancy_db_name')) = 'STRING' \
                THEN HEX(JSON_UNQUOTE(JSON_EXTRACT(t.data, '$.tenancy_db_name'))) ELSE '' END \
         ELSE '' END,\
         IF(BINARY t.id = BINARY @reprodb_lookup, 1, 0),\
         CASE WHEN t.data IS NULL OR NOT JSON_VALID(t.data) THEN 0 \
         WHEN (\
           (JSON_EXTRACT(t.data, '$.tenancy_db_connection') IS NOT NULL AND \
            JSON_TYPE(JSON_EXTRACT(t.data, '$.tenancy_db_connection')) <> 'NULL') OR \
           (JSON_EXTRACT(t.data, '$.tenancy_db_password') IS NOT NULL AND \
            JSON_TYPE(JSON_EXTRACT(t.data, '$.tenancy_db_password')) <> 'NULL') OR \
           (JSON_EXTRACT(t.data, '$.tenancy_db_host') IS NOT NULL AND \
            JSON_TYPE(JSON_EXTRACT(t.data, '$.tenancy_db_host')) <> 'NULL' AND \
            (JSON_TYPE(JSON_EXTRACT(t.data, '$.tenancy_db_host')) <> 'STRING' OR \
             BINARY JSON_UNQUOTE(JSON_EXTRACT(t.data, '$.tenancy_db_host')) <> BINARY @reprodb_source_host)) OR \
           (JSON_EXTRACT(t.data, '$.tenancy_db_port') IS NOT NULL AND \
            JSON_TYPE(JSON_EXTRACT(t.data, '$.tenancy_db_port')) <> 'NULL' AND \
            (JSON_TYPE(JSON_EXTRACT(t.data, '$.tenancy_db_port')) NOT IN ('INTEGER', 'STRING') OR \
             BINARY JSON_UNQUOTE(JSON_EXTRACT(t.data, '$.tenancy_db_port')) <> BINARY @reprodb_source_port)) OR \
           (JSON_EXTRACT(t.data, '$.tenancy_db_username') IS NOT NULL AND \
            JSON_TYPE(JSON_EXTRACT(t.data, '$.tenancy_db_username')) <> 'NULL' AND \
            (JSON_TYPE(JSON_EXTRACT(t.data, '$.tenancy_db_username')) <> 'STRING' OR \
             BINARY JSON_UNQUOTE(JSON_EXTRACT(t.data, '$.tenancy_db_username')) <> BINARY @reprodb_source_username))\
         ) THEN 1 ELSE 0 END,\
         {features} \
         FROM tenants AS t \
         WHERE BINARY t.id = BINARY @reprodb_lookup \
            OR ({domain_enabled} = 1 AND EXISTS (\
                SELECT 1 FROM domains AS d \
                WHERE d.tenant_id = t.id AND BINARY d.domain = BINARY @reprodb_lookup)) \
         ORDER BY HEX(t.id) LIMIT 2",
        features = feature_projection(),
    )
}

fn feature_projection() -> String {
    let mut columns = vec![
        "CASE \
         WHEN JSON_EXTRACT(t.data, '$.tenancy_app_color') IS NULL \
           OR JSON_TYPE(JSON_EXTRACT(t.data, '$.tenancy_app_color')) = 'NULL' THEN 'N' \
         WHEN JSON_TYPE(JSON_EXTRACT(t.data, '$.tenancy_app_color')) = 'STRING' \
           THEN CONCAT('S', HEX(JSON_UNQUOTE(JSON_EXTRACT(t.data, '$.tenancy_app_color')))) \
         ELSE 'X' END"
            .to_owned(),
    ];
    columns.extend(
        [
            "tenancy_annotation_atm",
            "tenancy_enable_stock_label_control",
            "tenancy_enable_sped_contrib",
            "tenancy_enable_beta",
            "tenancy_has_cyclic_counting",
            "tenancy_new_production",
        ]
        .into_iter()
        .map(|key| {
            format!(
                "CASE \
                 WHEN JSON_EXTRACT(t.data, '$.{key}') IS NULL \
                   OR JSON_TYPE(JSON_EXTRACT(t.data, '$.{key}')) = 'NULL' THEN 'N' \
                 WHEN JSON_TYPE(JSON_EXTRACT(t.data, '$.{key}')) = 'BOOLEAN' \
                   AND JSON_UNQUOTE(JSON_EXTRACT(t.data, '$.{key}')) = 'true' THEN 'T' \
                 WHEN JSON_TYPE(JSON_EXTRACT(t.data, '$.{key}')) = 'BOOLEAN' \
                   AND JSON_UNQUOTE(JSON_EXTRACT(t.data, '$.{key}')) = 'false' THEN 'F' \
                 ELSE 'X' END"
            )
        }),
    );
    columns.join(",")
}

fn parse_resolution(output: &[u8]) -> Result<ResolvedTenant, TenantResolutionError> {
    if output.len() > MAX_QUERY_OUTPUT_BYTES {
        return Err(TenantResolutionError::InvalidMetadata);
    }
    let output = std::str::from_utf8(output).map_err(|_| TenantResolutionError::InvalidMetadata)?;
    let rows = output.lines().collect::<Vec<_>>();
    match rows.len() {
        0 => return Err(TenantResolutionError::NotFound),
        1 => {}
        _ => return Err(TenantResolutionError::Ambiguous),
    }

    let columns = rows[0].split('\t').collect::<Vec<_>>();
    if columns.len() != 12 {
        return Err(TenantResolutionError::InvalidMetadata);
    }
    let tenant_id = decode_hex(columns[0], 255)
        .and_then(|value| TenantId::try_from(value).ok())
        .ok_or(TenantResolutionError::InvalidMetadata)?;
    let database = match columns[1] {
        "0" => DatabaseName::try_from(tenant_id.as_str())
            .map_err(TenantResolutionError::InvalidDatabase)?,
        "1" => {
            let value = decode_hex(columns[2], 64).ok_or(TenantResolutionError::InvalidMetadata)?;
            DatabaseName::try_from(value).map_err(TenantResolutionError::InvalidDatabase)?
        }
        "2" | "3" => return Err(TenantResolutionError::InvalidMetadata),
        _ => return Err(TenantResolutionError::InvalidMetadata),
    };
    let matched_by = match columns[3] {
        "0" => TenantMatch::Domain,
        "1" => TenantMatch::TenantId,
        _ => return Err(TenantResolutionError::InvalidMetadata),
    };
    match columns[4] {
        "0" => {}
        "1" => return Err(TenantResolutionError::ConnectionOverride),
        _ => return Err(TenantResolutionError::InvalidMetadata),
    }
    let features = LocalTenantFeatures {
        app_color: parse_app_color(columns[5])?,
        annotation_atm: parse_optional_bool(columns[6])?,
        enable_stock_label_control: parse_optional_bool(columns[7])?,
        enable_sped_contrib: parse_optional_bool(columns[8])?,
        enable_beta: parse_optional_bool(columns[9])?,
        has_cyclic_counting: parse_optional_bool(columns[10])?,
        new_production: parse_optional_bool(columns[11])?,
    };

    Ok(ResolvedTenant {
        tenant_id,
        database,
        matched_by,
        features,
    })
}

fn parse_app_color(value: &str) -> Result<Option<AppColor>, TenantResolutionError> {
    if value == "N" {
        return Ok(None);
    }
    let encoded = value
        .strip_prefix('S')
        .ok_or(TenantResolutionError::InvalidMetadata)?;
    let decoded = decode_hex(encoded, 32).ok_or(TenantResolutionError::InvalidMetadata)?;
    AppColor::try_from(decoded)
        .map(Some)
        .map_err(|_| TenantResolutionError::InvalidMetadata)
}

fn parse_optional_bool(value: &str) -> Result<Option<bool>, TenantResolutionError> {
    match value {
        "N" => Ok(None),
        "T" => Ok(Some(true)),
        "F" => Ok(Some(false)),
        _ => Err(TenantResolutionError::InvalidMetadata),
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

fn decode_hex(value: &str, max_decoded_bytes: usize) -> Option<String> {
    if !value.len().is_multiple_of(2) || value.len() > max_decoded_bytes * 2 {
        return None;
    }
    let mut decoded = Vec::with_capacity(value.len() / 2);
    for pair in value.as_bytes().chunks_exact(2) {
        let high = hex_digit(pair[0])?;
        let low = hex_digit(pair[1])?;
        decoded.push((high << 4) | low);
    }
    String::from_utf8(decoded).ok()
}

fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn map_client_error(error: DockerClientError) -> TenantResolutionError {
    match error {
        DockerClientError::AuthenticationFailed => TenantResolutionError::AuthenticationFailed,
        DockerClientError::Catalog(_)
        | DockerClientError::ImageUnavailable
        | DockerClientError::ImagePullFailed
        | DockerClientError::ImageInspectFailed
        | DockerClientError::InvalidImageMetadata
        | DockerClientError::ImageDigestMismatch
        | DockerClientError::VersionProbeFailed
        | DockerClientError::IncompatibleClientVersion => TenantResolutionError::ClientUnavailable,
        DockerClientError::DockerUnavailable
        | DockerClientError::InvalidDockerContext
        | DockerClientError::Process(_)
        | DockerClientError::SourceNetworkUnavailable
        | DockerClientError::TlsValidationFailed
        | DockerClientError::ConnectionProbeFailed => TenantResolutionError::SourceUnavailable,
        DockerClientError::OptionFile(_)
        | DockerClientError::OptionFilePathNotAbsolute
        | DockerClientError::QueryFailed
        | DockerClientError::InvalidServerMetadata => TenantResolutionError::InvalidMetadata,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        process::{Command, Stdio},
        sync::{Arc, Mutex},
    };

    use crate::{
        domain::ValueObjectError,
        infrastructure::{
            docker::DockerTargetDiscovery,
            mysql::ClientCatalog,
            process::{
                ProcessError, ProcessOutput, ProcessRunner, ProcessSpec, TokioProcessRunner,
            },
        },
    };
    use secrecy::zeroize::Zeroize;
    use uuid::Uuid;

    use super::*;

    const NO_FEATURES: &str = "\tN\tN\tN\tN\tN\tN\tN";

    struct FakeRunner {
        outputs: Mutex<VecDeque<ProcessOutput>>,
        commands: Arc<Mutex<Vec<ProcessSpec>>>,
    }

    #[async_trait]
    impl ProcessRunner for FakeRunner {
        async fn output(&self, spec: &ProcessSpec) -> Result<ProcessOutput, ProcessError> {
            self.commands.lock().unwrap().push(spec.clone());
            Ok(self.outputs.lock().unwrap().pop_front().unwrap())
        }
    }

    fn resolver(
        query_output: impl Into<Vec<u8>>,
        allow_domains: bool,
    ) -> (
        DockerSaltCentralTenantResolver<FakeRunner>,
        Arc<Mutex<Vec<ProcessSpec>>>,
    ) {
        let client = ClientCatalog::resolve("8.4").unwrap();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let resolver = DockerSaltCentralTenantResolver::new(
            FakeRunner {
                outputs: Mutex::new(VecDeque::from([
                    ProcessOutput::success(
                        serde_json::to_vec(&vec![client.repository_digest()]).unwrap(),
                    ),
                    ProcessOutput::success("mysql  Ver 8.4.4 for Linux on aarch64\n"),
                    ProcessOutput::success(query_output),
                ])),
                commands: Arc::clone(&commands),
            },
            SaltCentralSource {
                docker_context: "desktop-linux".to_owned(),
                host: "127.0.0.1".to_owned(),
                port: 3306,
                username: "readonly".to_owned(),
                password: SecretString::from("password-marker"),
                tls_mode: MysqlTlsMode::Required,
                tls_material: Default::default(),
                central_database: DatabaseName::try_from("salt_central").unwrap(),
                client,
            },
            allow_domains,
        );
        (resolver, commands)
    }

    #[tokio::test]
    async fn resolves_a_domain_without_returning_the_central_json() {
        let (resolver, commands) = resolver(
            format!("73616C745F73616761746563\t0\t\t0\t0{NO_FEATURES}\n"),
            true,
        );
        let lookup = TenantLookup::try_from("sagatec").unwrap();

        let resolved = resolver.resolve(&lookup).await.unwrap();

        assert_eq!(resolved.tenant_id.as_str(), "salt_sagatec");
        assert_eq!(resolved.database.as_str(), "salt_sagatec");
        assert_eq!(resolved.matched_by, TenantMatch::Domain);
        let commands = commands.lock().unwrap();
        let arguments = commands.last().unwrap().arguments();
        let query = arguments.last().unwrap().to_string_lossy();
        assert!(query.contains("0x73616761746563"));
        assert!(!query.contains("sagatec"));
        assert!(!query.contains("readonly"));
        assert!(
            arguments
                .iter()
                .any(|argument| argument == "--database=salt_central")
        );
        assert!(
            !arguments
                .iter()
                .any(|argument| argument.to_string_lossy().contains("password-marker"))
        );
    }

    #[tokio::test]
    async fn respects_a_valid_database_override() {
        let (resolver, _) = resolver(
            format!(
                "73616C745F706F6C796D6572\t1\t73616C745F706F6C796D65725F64617461\t1\t0{NO_FEATURES}\n"
            ),
            true,
        );

        let resolved = resolver
            .resolve(&TenantLookup::try_from("salt_polymer").unwrap())
            .await
            .unwrap();

        assert_eq!(resolved.tenant_id.as_str(), "salt_polymer");
        assert_eq!(resolved.database.as_str(), "salt_polymer_data");
        assert_eq!(resolved.matched_by, TenantMatch::TenantId);
    }

    #[tokio::test]
    async fn reports_not_found_ambiguity_and_invalid_json_without_leaking_rows() {
        let lookup = TenantLookup::try_from("sagatec").unwrap();
        assert_eq!(
            resolver(Vec::new(), true)
                .0
                .resolve(&lookup)
                .await
                .unwrap_err(),
            TenantResolutionError::NotFound
        );
        assert_eq!(
            resolver(
                format!(
                    "73616C745F61\t0\t\t0\t0{NO_FEATURES}\n73616C745F62\t0\t\t0\t0{NO_FEATURES}\n"
                ),
                true,
            )
            .0
            .resolve(&lookup)
            .await
            .unwrap_err(),
            TenantResolutionError::Ambiguous
        );
        let error = resolver(
            format!("73616C745F73616761746563\t3\t\t0\t0{NO_FEATURES}\n"),
            true,
        )
        .0
        .resolve(&lookup)
        .await
        .unwrap_err();
        assert_eq!(error, TenantResolutionError::InvalidMetadata);
        assert!(!error.to_string().contains("salt_sagatec"));
    }

    #[tokio::test]
    async fn rejects_invalid_and_administrative_database_overrides() {
        let lookup = TenantLookup::try_from("sagatec").unwrap();
        for database_hex in ["2E2E2F78", "6D7973716C"] {
            let output =
                format!("73616C745F73616761746563\t1\t{database_hex}\t0\t0{NO_FEATURES}\n");
            assert!(matches!(
                resolver(output, true).0.resolve(&lookup).await,
                Err(TenantResolutionError::InvalidDatabase(_))
            ));
        }
    }

    #[test]
    fn domain_lookup_is_explicitly_switchable_in_the_fixed_query() {
        let lookup = TenantLookup::try_from("sagatec").unwrap();

        assert!(
            resolution_query(&lookup, true, "127.0.0.1", 3306, "readonly").contains("OR (1 = 1")
        );
        assert!(
            resolution_query(&lookup, false, "127.0.0.1", 3306, "readonly").contains("OR (0 = 1")
        );
    }

    #[tokio::test]
    async fn blocks_connection_overrides_without_returning_their_values() {
        let marker = "production-password-marker";
        let lookup = TenantLookup::try_from("sagatec").unwrap();
        let error = resolver(
            format!("73616C745F73616761746563\t0\t\t0\t1{NO_FEATURES}\n"),
            true,
        )
        .0
        .resolve(&lookup)
        .await
        .unwrap_err();

        assert_eq!(error, TenantResolutionError::ConnectionOverride);
        assert!(!error.to_string().contains(marker));
        assert!(!format!("{error:?}").contains(marker));
    }

    #[test]
    fn schema_query_failures_are_metadata_errors_not_network_errors() {
        assert_eq!(
            map_client_error(DockerClientError::QueryFailed),
            TenantResolutionError::InvalidMetadata
        );
    }

    #[tokio::test]
    async fn returns_only_allowlisted_typed_features() {
        let (resolver, _) = resolver(
            "73616C745F73616761746563\t0\t\t0\t0\tS677265656E\tT\tF\tN\tT\tF\tN\n",
            true,
        );

        let resolved = resolver
            .resolve(&TenantLookup::try_from("sagatec").unwrap())
            .await
            .unwrap();

        assert_eq!(resolved.features.app_color.unwrap().as_str(), "green");
        assert_eq!(resolved.features.annotation_atm, Some(true));
        assert_eq!(resolved.features.enable_stock_label_control, Some(false));
        assert_eq!(resolved.features.enable_sped_contrib, None);
        assert_eq!(resolved.features.enable_beta, Some(true));
        assert_eq!(resolved.features.has_cyclic_counting, Some(false));
        assert_eq!(resolved.features.new_production, None);
    }

    #[test]
    fn central_fixture_contains_only_explicitly_synthetic_sensitive_values() {
        let fixture = include_str!("../../../tests/fixtures/salt_central/tenant_resolution.sql");

        for required_case in [
            "salt_by_id",
            "salt_sagatec",
            "salt_polymer_data",
            "salt_sensitive",
            "salt_admin_override",
            "salt_collision_domain",
            "tenant_links",
        ] {
            assert!(fixture.contains(required_case));
        }
        assert!(fixture.contains("production.fixture.invalid"));
        assert!(fixture.contains("fixture-only-password-do-not-use"));
        assert!(fixture.contains("fixture-only-token-do-not-use"));
    }

    #[tokio::test]
    #[ignore = "creates and removes a unique fixture database in the local mysql-8 container"]
    async fn resolves_the_sanitized_central_fixture_end_to_end() {
        let expected =
            std::env::var("REPRODB_TEST_MYSQL_CONTAINER").unwrap_or_else(|_| "mysql-8".to_owned());
        let discovery = DockerTargetDiscovery::new(TokioProcessRunner);
        let (context, candidates) = discovery.discover().await.unwrap();
        let candidate = candidates
            .into_iter()
            .find(|candidate| candidate.name.as_str() == expected)
            .expect("the expected local MySQL container was not discovered");
        let host_port = candidate
            .published_ports
            .first()
            .expect("the fixture container must publish MySQL")
            .host_port;
        let password = local_container_root_password(&context, &expected);
        let client = ClientCatalog::resolve("8.4").unwrap();
        let runtime = DockerMysqlClientRuntime::new(TokioProcessRunner);
        let prepared = runtime
            .prepare_existing(&context, client.series(), client.image())
            .await
            .unwrap();
        let option_file = runtime
            .create_option_file(
                "127.0.0.1",
                host_port,
                "root",
                &password,
                MysqlTlsMode::Required,
            )
            .unwrap();
        let control_database = DatabaseName::try_from("salt_central").unwrap();
        let fixture_database =
            DatabaseName::try_from(format!("reprodb_fixture_{}", Uuid::new_v4().simple())).unwrap();
        let create = format!(
            "CREATE DATABASE `{}` CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
            fixture_database.as_str()
        );
        runtime
            .query_connection(&prepared, &option_file, &control_database, &create)
            .await
            .unwrap();

        let fixture_result = runtime
            .query_connection(
                &prepared,
                &option_file,
                &fixture_database,
                include_str!("../../../tests/fixtures/salt_central/tenant_resolution.sql"),
            )
            .await;
        let results = if fixture_result.is_ok() {
            let resolver = DockerSaltCentralTenantResolver::new(
                TokioProcessRunner,
                SaltCentralSource {
                    docker_context: context,
                    host: "127.0.0.1".to_owned(),
                    port: host_port,
                    username: "root".to_owned(),
                    password: password.clone(),
                    tls_mode: MysqlTlsMode::Required,
                    tls_material: Default::default(),
                    central_database: fixture_database.clone(),
                    client,
                },
                true,
            );
            Some((
                runtime
                    .query_connection(
                        &prepared,
                        &option_file,
                        &fixture_database,
                        "SELECT COUNT(*) FROM tenant_links",
                    )
                    .await,
                resolver
                    .resolve(&TenantLookup::try_from("salt_by_id").unwrap())
                    .await,
                resolver
                    .resolve(&TenantLookup::try_from("sagatec").unwrap())
                    .await,
                resolver
                    .resolve(&TenantLookup::try_from("polymer").unwrap())
                    .await,
                resolver
                    .resolve(&TenantLookup::try_from("sensitive").unwrap())
                    .await,
                resolver
                    .resolve(&TenantLookup::try_from("admin-override").unwrap())
                    .await,
                resolver
                    .resolve(&TenantLookup::try_from("collision").unwrap())
                    .await,
            ))
        } else {
            None
        };

        let drop = format!("DROP DATABASE `{}`", fixture_database.as_str());
        let cleanup_result = runtime
            .query_connection(&prepared, &option_file, &control_database, &drop)
            .await;
        cleanup_result.expect("the unique fixture database must be removed");
        fixture_result.expect("the sanitized central fixture must load");

        let (
            tenant_link_count,
            by_id,
            by_domain,
            override_database,
            sensitive,
            administrative,
            collision,
        ) = results.unwrap();
        assert_eq!(tenant_link_count.unwrap(), b"1\n");
        assert_resolution(
            by_id.unwrap(),
            "salt_by_id",
            "salt_by_id",
            TenantMatch::TenantId,
        );
        assert_resolution(
            by_domain.unwrap(),
            "salt_sagatec",
            "salt_sagatec",
            TenantMatch::Domain,
        );
        assert_resolution(
            override_database.unwrap(),
            "salt_polymer",
            "salt_polymer_data",
            TenantMatch::Domain,
        );
        let public_results = format!("{sensitive:?}{administrative:?}{collision:?}");
        assert_eq!(
            sensitive.unwrap_err(),
            TenantResolutionError::ConnectionOverride
        );
        assert!(matches!(
            administrative.unwrap_err(),
            TenantResolutionError::InvalidDatabase(ValueObjectError::Reserved { .. })
        ));
        assert_eq!(collision.unwrap_err(), TenantResolutionError::Ambiguous);

        assert!(!public_results.contains("fixture-only-password-do-not-use"));
        assert!(!public_results.contains("fixture-only-token-do-not-use"));
    }

    fn assert_resolution(
        resolution: ResolvedTenant,
        tenant_id: &str,
        database: &str,
        matched_by: TenantMatch,
    ) {
        assert_eq!(resolution.tenant_id.as_str(), tenant_id);
        assert_eq!(resolution.database.as_str(), database);
        assert_eq!(resolution.matched_by, matched_by);
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
}
