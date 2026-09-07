use async_trait::async_trait;

use crate::{
    application::{
        TenantCatalogEntry, TenantCatalogPage, TenantCatalogReadError, TenantCatalogReader,
        TenantCatalogSource,
    },
    domain::{DatabaseName, TenantId},
    infrastructure::{mysql::DockerClientError, process::ProcessRunner},
};

use super::DockerMysqlClientRuntime;

const MAX_CATALOG_OUTPUT_BYTES: usize = 1024 * 1024;

pub struct DockerTenantCatalogReader<R> {
    runtime: DockerMysqlClientRuntime<R>,
}

impl<R> DockerTenantCatalogReader<R>
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
impl<R> TenantCatalogReader for DockerTenantCatalogReader<R>
where
    R: ProcessRunner,
{
    async fn list(
        &self,
        source: &TenantCatalogSource,
        limit: u16,
    ) -> Result<TenantCatalogPage, TenantCatalogReadError> {
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
            .query_connection(
                &prepared,
                &option_file,
                &source.central_database,
                &catalog_query(requested),
            )
            .await
            .map_err(map_client_error)?;
        let (entries, truncated) = parse_catalog(&output, usize::from(limit))?;

        Ok(TenantCatalogPage {
            profile_name: source.profile_name.clone(),
            central_database: source.central_database.clone(),
            entries,
            truncated,
        })
    }
}

fn catalog_query(limit: u32) -> String {
    format!(
        "SELECT HEX(t.id),\
         CASE WHEN t.data IS NULL THEN 0 \
              WHEN JSON_VALID(t.data) THEN \
                CASE WHEN JSON_EXTRACT(t.data, '$.tenancy_db_name') IS NULL THEN 0 \
                     WHEN JSON_TYPE(JSON_EXTRACT(t.data, '$.tenancy_db_name')) = 'STRING' THEN 1 \
                     ELSE 2 END \
              ELSE 3 END,\
         CASE WHEN t.data IS NOT NULL AND JSON_VALID(t.data) \
                   AND JSON_TYPE(JSON_EXTRACT(t.data, '$.tenancy_db_name')) = 'STRING' \
              THEN HEX(JSON_UNQUOTE(JSON_EXTRACT(t.data, '$.tenancy_db_name'))) ELSE '' END,\
         COALESCE((SELECT HEX(d.domain) FROM domains AS d \
                   WHERE d.tenant_id = t.id ORDER BY HEX(d.domain) LIMIT 1), '') \
         FROM tenants AS t ORDER BY HEX(t.id) LIMIT {limit}"
    )
}

fn parse_catalog(
    output: &[u8],
    limit: usize,
) -> Result<(Vec<TenantCatalogEntry>, bool), TenantCatalogReadError> {
    if output.len() > MAX_CATALOG_OUTPUT_BYTES {
        return Err(TenantCatalogReadError::InvalidMetadata);
    }
    let output =
        std::str::from_utf8(output).map_err(|_| TenantCatalogReadError::InvalidMetadata)?;
    let mut entries = Vec::new();
    for row in output.lines().take(limit + 1) {
        let columns = row.split('\t').collect::<Vec<_>>();
        let [tenant_id, database_kind, database_override, primary_domain] = columns.as_slice()
        else {
            return Err(TenantCatalogReadError::InvalidMetadata);
        };
        let tenant_id = decode_hex(tenant_id, 255)
            .and_then(|value| TenantId::try_from(value).ok())
            .ok_or(TenantCatalogReadError::InvalidMetadata)?;
        let database = match *database_kind {
            "0" => DatabaseName::try_from(tenant_id.as_str())
                .map_err(|_| TenantCatalogReadError::InvalidMetadata)?,
            "1" => decode_hex(database_override, 64)
                .and_then(|value| DatabaseName::try_from(value).ok())
                .ok_or(TenantCatalogReadError::InvalidMetadata)?,
            _ => return Err(TenantCatalogReadError::InvalidMetadata),
        };
        let primary_domain = if primary_domain.is_empty() {
            None
        } else {
            let value =
                decode_hex(primary_domain, 255).ok_or(TenantCatalogReadError::InvalidMetadata)?;
            if value.is_empty() || value.chars().any(char::is_control) {
                return Err(TenantCatalogReadError::InvalidMetadata);
            }
            Some(value)
        };
        entries.push(TenantCatalogEntry {
            tenant_id,
            database,
            primary_domain,
        });
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

fn map_client_error(error: DockerClientError) -> TenantCatalogReadError {
    match error {
        DockerClientError::AuthenticationFailed => TenantCatalogReadError::AuthenticationFailed,
        DockerClientError::Catalog(_)
        | DockerClientError::ImageUnavailable
        | DockerClientError::ImagePullFailed
        | DockerClientError::ImageInspectFailed
        | DockerClientError::InvalidImageMetadata
        | DockerClientError::ImageDigestMismatch
        | DockerClientError::VersionProbeFailed
        | DockerClientError::IncompatibleClientVersion => TenantCatalogReadError::ClientUnavailable,
        DockerClientError::DockerUnavailable
        | DockerClientError::InvalidDockerContext
        | DockerClientError::Process(_)
        | DockerClientError::SourceNetworkUnavailable
        | DockerClientError::TlsValidationFailed
        | DockerClientError::ConnectionProbeFailed => TenantCatalogReadError::SourceUnavailable,
        DockerClientError::QueryFailed => TenantCatalogReadError::SchemaUnavailable,
        DockerClientError::OptionFile(_)
        | DockerClientError::OptionFilePathNotAbsolute
        | DockerClientError::InvalidServerMetadata => TenantCatalogReadError::InvalidMetadata,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_allowlisted_catalog_fields_and_honors_the_limit() {
        let output = concat!(
            "73616C745F73616761746563\t0\t\t73616761746563\n",
            "73616C745F706F6C796D6572\t1\t73616C745F706F6C796D65725F64656D6F\t706F6C796D6572\n",
        );

        let (entries, truncated) = parse_catalog(output.as_bytes(), 1).unwrap();

        assert!(truncated);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].tenant_id.as_str(), "salt_sagatec");
        assert_eq!(entries[0].database.as_str(), "salt_sagatec");
        assert_eq!(entries[0].primary_domain.as_deref(), Some("sagatec"));
        let query = catalog_query(101);
        assert!(query.starts_with("SELECT "));
        assert!(!query.contains("SELECT t.data"));
        assert!(!query.contains("tenancy_db_password"));
        for mutation in [
            "INSERT ", "UPDATE ", "DELETE ", "DROP ", "ALTER ", "CREATE ",
        ] {
            assert!(!query.contains(mutation));
        }
        assert!(query.ends_with("LIMIT 101"));
    }

    #[test]
    fn rejects_invalid_json_types_identifiers_and_untrusted_output() {
        for output in [
            b"73616C745F73616761746563\t2\t\t\n".as_slice(),
            b"2E2E2F\t0\t\t\n".as_slice(),
            b"not-hex\t0\t\t\n".as_slice(),
            b"73616C745F73616761746563\t0\t\t00\n".as_slice(),
        ] {
            assert_eq!(
                parse_catalog(output, 100).unwrap_err(),
                TenantCatalogReadError::InvalidMetadata
            );
        }
    }
}
