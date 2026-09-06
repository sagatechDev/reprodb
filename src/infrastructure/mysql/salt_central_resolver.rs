use async_trait::async_trait;
use secrecy::SecretString;

use crate::{
    domain::{
        DatabaseName, MysqlTlsMode, ResolvedTenant, TenantId, TenantLookup, TenantMatch,
        TenantResolutionError, TenantResolver,
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
            .create_option_file(
                &self.source.host,
                self.source.port,
                &self.source.username,
                &self.source.password,
                self.source.tls_mode,
            )
            .map_err(map_client_error)?;
        let query = resolution_query(lookup, self.allow_domain_lookup);
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

fn resolution_query(lookup: &TenantLookup, allow_domain_lookup: bool) -> String {
    let lookup = encode_hex(lookup.as_str().as_bytes());
    let domain_enabled = u8::from(allow_domain_lookup);
    format!(
        "SET @reprodb_lookup = CONVERT(0x{lookup} USING utf8mb4);\
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
         IF(BINARY t.id = BINARY @reprodb_lookup, 1, 0) \
         FROM tenants AS t \
         WHERE BINARY t.id = BINARY @reprodb_lookup \
            OR ({domain_enabled} = 1 AND EXISTS (\
                SELECT 1 FROM domains AS d \
                WHERE d.tenant_id = t.id AND BINARY d.domain = BINARY @reprodb_lookup)) \
         ORDER BY HEX(t.id) LIMIT 2"
    )
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
    if columns.len() != 4 {
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

    Ok(ResolvedTenant {
        tenant_id,
        database,
        matched_by,
    })
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
        sync::{Arc, Mutex},
    };

    use crate::infrastructure::{
        mysql::ClientCatalog,
        process::{ProcessError, ProcessOutput, ProcessSpec},
    };

    use super::*;

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
                central_database: DatabaseName::try_from("salt_central").unwrap(),
                client,
            },
            allow_domains,
        );
        (resolver, commands)
    }

    #[tokio::test]
    async fn resolves_a_domain_without_returning_the_central_json() {
        let (resolver, commands) = resolver("73616C745F73616761746563\t0\t\t0\n", true);
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
            "73616C745F706F6C796D6572\t1\t73616C745F706F6C796D65725F64617461\t1\n",
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
            resolver("73616C745F61\t0\t\t0\n73616C745F62\t0\t\t0\n", true)
                .0
                .resolve(&lookup)
                .await
                .unwrap_err(),
            TenantResolutionError::Ambiguous
        );
        let error = resolver("73616C745F73616761746563\t3\t\t0\n", true)
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
            let output = format!("73616C745F73616761746563\t1\t{database_hex}\t0\n");
            assert!(matches!(
                resolver(output, true).0.resolve(&lookup).await,
                Err(TenantResolutionError::InvalidDatabase(_))
            ));
        }
    }

    #[test]
    fn domain_lookup_is_explicitly_switchable_in_the_fixed_query() {
        let lookup = TenantLookup::try_from("sagatec").unwrap();

        assert!(resolution_query(&lookup, true).contains("OR (1 = 1"));
        assert!(resolution_query(&lookup, false).contains("OR (0 = 1"));
    }

    #[test]
    fn schema_query_failures_are_metadata_errors_not_network_errors() {
        assert_eq!(
            map_client_error(DockerClientError::QueryFailed),
            TenantResolutionError::InvalidMetadata
        );
    }
}
