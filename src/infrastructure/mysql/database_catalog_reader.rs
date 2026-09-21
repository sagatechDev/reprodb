use async_trait::async_trait;

use crate::{
    application::{
        DatabaseCatalogEntry, DatabaseCatalogPage, DatabaseCatalogReadError, DatabaseCatalogReader,
        DatabaseCatalogSource,
    },
    infrastructure::{mysql::DockerClientError, process::ProcessRunner},
};

use super::DockerMysqlClientRuntime;

const MAX_CATALOG_OUTPUT_BYTES: usize = 1024 * 1024;

pub struct DockerDatabaseCatalogReader<R> {
    runtime: DockerMysqlClientRuntime<R>,
}

impl<R> DockerDatabaseCatalogReader<R>
where
    R: ProcessRunner,
{
    pub fn new(runner: R) -> Self {
        Self {
            runtime: DockerMysqlClientRuntime::new(runner),
        }
    }
}

#[async_trait]
impl<R> DatabaseCatalogReader for DockerDatabaseCatalogReader<R>
where
    R: ProcessRunner,
{
    async fn list(
        &self,
        source: &DatabaseCatalogSource,
        limit: u16,
    ) -> Result<DatabaseCatalogPage, DatabaseCatalogReadError> {
        let prepared = self
            .runtime
            .prepare_existing(
                &source.docker_context,
                source.client.series(),
                source.client.image(),
            )
            .await
            .map_err(map_client_error)?;
        let option_file = self
            .runtime
            .create_option_file_with_tls_material(
                &source.host,
                source.port,
                &source.username,
                &source.password,
                source.tls_mode,
                &source.tls_material,
            )
            .map_err(map_client_error)?;
        let requested = u32::from(limit) + 1;
        let output = self
            .runtime
            .query_server(&prepared, &option_file, &catalog_query(requested))
            .await
            .map_err(map_client_error)?;
        let (entries, truncated) = parse_catalog(&output, usize::from(limit))?;

        Ok(DatabaseCatalogPage {
            profile_name: source.profile_name.clone(),
            entries,
            truncated,
        })
    }
}

fn catalog_query(limit: u32) -> String {
    format!(
        "SELECT HEX(SCHEMA_NAME) FROM information_schema.SCHEMATA \
         WHERE LOWER(SCHEMA_NAME) NOT IN \
         ('mysql','information_schema','performance_schema','sys') \
         ORDER BY SCHEMA_NAME LIMIT {limit}"
    )
}

fn parse_catalog(
    output: &[u8],
    limit: usize,
) -> Result<(Vec<DatabaseCatalogEntry>, bool), DatabaseCatalogReadError> {
    if output.len() > MAX_CATALOG_OUTPUT_BYTES {
        return Err(DatabaseCatalogReadError::InvalidMetadata);
    }
    let output =
        std::str::from_utf8(output).map_err(|_| DatabaseCatalogReadError::InvalidMetadata)?;
    let mut entries = Vec::new();
    for row in output.lines().take(limit + 1) {
        if row.contains('\t') {
            return Err(DatabaseCatalogReadError::InvalidMetadata);
        }
        let database = decode_hex(row, 64)
            .filter(|value| !value.is_empty() && !value.chars().any(char::is_control))
            .ok_or(DatabaseCatalogReadError::InvalidMetadata)?;
        entries.push(DatabaseCatalogEntry { database });
    }
    let truncated = entries.len() > limit;
    entries.truncate(limit);
    Ok((entries, truncated))
}

fn decode_hex(value: &str, max_decoded_bytes: usize) -> Option<String> {
    if !value.len().is_multiple_of(2) || value.len() > max_decoded_bytes * 2 {
        return None;
    }
    let mut decoded = Vec::with_capacity(value.len() / 2);
    for pair in value.as_bytes().as_chunks::<2>().0 {
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

fn map_client_error(error: DockerClientError) -> DatabaseCatalogReadError {
    match error {
        DockerClientError::AuthenticationFailed => DatabaseCatalogReadError::AuthenticationFailed,
        DockerClientError::Catalog(_)
        | DockerClientError::ImageUnavailable
        | DockerClientError::ImagePullFailed
        | DockerClientError::ImageInspectFailed
        | DockerClientError::InvalidImageMetadata
        | DockerClientError::ImageDigestMismatch
        | DockerClientError::VersionProbeFailed
        | DockerClientError::IncompatibleClientVersion => {
            DatabaseCatalogReadError::ClientUnavailable
        }
        DockerClientError::DockerUnavailable
        | DockerClientError::InvalidDockerContext
        | DockerClientError::Process(_)
        | DockerClientError::SourceNetworkUnavailable
        | DockerClientError::TlsValidationFailed
        | DockerClientError::ConnectionProbeFailed => DatabaseCatalogReadError::SourceUnavailable,
        DockerClientError::QueryFailed => DatabaseCatalogReadError::SchemaUnavailable,
        DockerClientError::OptionFile(_)
        | DockerClientError::OptionFilePathNotAbsolute
        | DockerClientError::InvalidServerMetadata => DatabaseCatalogReadError::InvalidMetadata,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_allowlisted_catalog_fields_and_honors_the_limit() {
        let output = concat!("64656D6F5F61636D65\n", "64656D6F5F676C6F626578\n");

        let (entries, truncated) = parse_catalog(output.as_bytes(), 1).unwrap();

        assert!(truncated);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].database.as_str(), "demo_acme");
        let query = catalog_query(101);
        assert!(query.starts_with("SELECT "));
        assert!(query.contains("information_schema.SCHEMATA"));
        assert!(!query.contains("tenants"));
        assert!(!query.contains("domains"));
        for mutation in [
            "INSERT ", "UPDATE ", "DELETE ", "DROP ", "ALTER ", "CREATE ",
        ] {
            assert!(!query.contains(mutation));
        }
        assert!(query.ends_with("LIMIT 101"));
    }

    #[test]
    fn preserves_database_names_returned_by_mysql_without_applying_cli_identifier_rules() {
        let output = "64656D6F2D6C6567616379\n";

        let (entries, truncated) = parse_catalog(output.as_bytes(), 100).unwrap();

        assert!(!truncated);
        assert_eq!(entries[0].database, "demo-legacy");
    }

    #[test]
    fn rejects_invalid_json_types_identifiers_and_untrusted_output() {
        for output in [
            b"64656D6F5F73616761746563\textra\n".as_slice(),
            b"2E2E2F\t0\t\t\n".as_slice(),
            b"not-hex\t0\t\t\n".as_slice(),
            b"00\n".as_slice(),
        ] {
            assert_eq!(
                parse_catalog(output, 100).unwrap_err(),
                DatabaseCatalogReadError::InvalidMetadata
            );
        }
    }
}
