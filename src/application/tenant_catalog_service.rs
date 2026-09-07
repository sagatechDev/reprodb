use async_trait::async_trait;
use secrecy::SecretString;
use thiserror::Error;

use crate::{
    domain::{MysqlTlsMaterialPaths, MysqlTlsMode},
    infrastructure::{
        config::{ConfigError, ConfigRepository},
        credentials::{CredentialError, CredentialStore},
        mysql::{ApprovedMysqlClient, ClientCatalog, ClientCatalogError},
    },
};

pub const DEFAULT_TENANT_LIST_LIMIT: u16 = 100;
pub const MAX_TENANT_LIST_LIMIT: u16 = 500;

pub struct TenantCatalogSource {
    pub profile_name: String,
    pub docker_context: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: SecretString,
    pub tls_mode: MysqlTlsMode,
    pub tls_material: MysqlTlsMaterialPaths,
    pub client: ApprovedMysqlClient,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TenantCatalogEntry {
    pub database: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TenantCatalogPage {
    pub profile_name: String,
    pub entries: Vec<TenantCatalogEntry>,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum TenantCatalogReadError {
    #[error("the approved MySQL client is unavailable; run `reprodb doctor`")]
    ClientUnavailable,
    #[error("the tenant catalog source could not be reached")]
    SourceUnavailable,
    #[error("the tenant catalog source rejected the configured credential")]
    AuthenticationFailed,
    #[error("the source database catalog is unavailable")]
    SchemaUnavailable,
    #[error("the source returned an invalid database name")]
    InvalidMetadata,
}

#[async_trait]
pub trait TenantCatalogReader: Send + Sync {
    async fn list(
        &self,
        source: &TenantCatalogSource,
        limit: u16,
    ) -> Result<TenantCatalogPage, TenantCatalogReadError>;
}

#[derive(Debug, Error)]
pub enum TenantCatalogServiceError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("no active source profile; run `reprodb profile use NAME`")]
    NoActiveProfile,
    #[error("database list limit must be between 1 and {MAX_TENANT_LIST_LIMIT}")]
    InvalidLimit,
    #[error(transparent)]
    Credential(#[from] CredentialError),
    #[error(transparent)]
    Client(#[from] ClientCatalogError),
    #[error(transparent)]
    Read(#[from] TenantCatalogReadError),
}

pub struct TenantCatalogService {
    repository: ConfigRepository,
}

impl TenantCatalogService {
    pub fn new(repository: ConfigRepository) -> Self {
        Self { repository }
    }

    pub async fn list(
        &self,
        credentials: &dyn CredentialStore,
        reader: &dyn TenantCatalogReader,
        limit: u16,
    ) -> Result<TenantCatalogPage, TenantCatalogServiceError> {
        if limit == 0 || limit > MAX_TENANT_LIST_LIMIT {
            return Err(TenantCatalogServiceError::InvalidLimit);
        }
        let config = self.repository.load()?;
        let profile_name = config
            .active_profile
            .as_ref()
            .ok_or(TenantCatalogServiceError::NoActiveProfile)?;
        let profile = config
            .profiles
            .get(profile_name)
            .ok_or(TenantCatalogServiceError::NoActiveProfile)?;
        let docker_context = config
            .client_runtime
            .docker_context
            .clone()
            .ok_or(TenantCatalogServiceError::NoActiveProfile)?;
        let password = credentials.get(&profile.credential_key).await?;
        let client = ClientCatalog::validate(&profile.mysql_series, &profile.client.image)?;

        reader
            .list(
                &TenantCatalogSource {
                    profile_name: profile_name.as_str().to_owned(),
                    docker_context,
                    host: profile.host.clone(),
                    port: profile.port,
                    username: profile.username.clone(),
                    password,
                    tls_mode: profile.tls_mode,
                    tls_material: profile.tls_material.clone(),
                    client,
                },
                limit,
            )
            .await
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use async_trait::async_trait;
    use secrecy::SecretString;
    use tempfile::TempDir;

    use crate::{
        domain::{CredentialKey, CredentialScope, DatabaseName, MysqlTlsMode, ProfileName},
        infrastructure::{
            config::{
                AppConfig, AppPaths, ClientRuntimeConfig, MysqlClientConfig, MysqlFamily,
                SourceProfileConfig, TenantResolverConfig,
            },
            credentials::MemoryCredentialStore,
        },
    };

    use super::*;

    struct FakeReader {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl TenantCatalogReader for FakeReader {
        async fn list(
            &self,
            source: &TenantCatalogSource,
            limit: u16,
        ) -> Result<TenantCatalogPage, TenantCatalogReadError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(source.profile_name, "sandbox");
            assert_eq!(source.docker_context, "desktop-linux");
            assert_eq!(limit, 25);
            Ok(TenantCatalogPage {
                profile_name: source.profile_name.clone(),
                entries: Vec::new(),
                truncated: false,
            })
        }
    }

    fn repository(temp: &TempDir) -> (ConfigRepository, CredentialKey) {
        let repository = ConfigRepository::new(AppPaths::from_root(temp.path()));
        let name = ProfileName::try_from("sandbox").unwrap();
        let credential_key = CredentialKey::new(CredentialScope::Source);
        let client = ClientCatalog::resolve("8.4").unwrap();
        repository
            .save(&AppConfig {
                active_profile: Some(name.clone()),
                client_runtime: ClientRuntimeConfig {
                    docker_context: Some("desktop-linux".to_owned()),
                    ..Default::default()
                },
                profiles: BTreeMap::from([(
                    name,
                    SourceProfileConfig {
                        host: "sandbox.internal".to_owned(),
                        port: 3306,
                        username: "readonly".to_owned(),
                        credential_key,
                        mysql_family: MysqlFamily::Mysql,
                        mysql_series: "8.4".to_owned(),
                        production: false,
                        tls_mode: MysqlTlsMode::Required,
                        tls_material: Default::default(),
                        client: MysqlClientConfig {
                            image: client.image().to_owned(),
                        },
                        tenant_resolver: TenantResolverConfig::SaltCentral {
                            central_database: DatabaseName::try_from("sandbox_central").unwrap(),
                            allow_domain_lookup: true,
                        },
                    },
                )]),
                ..AppConfig::default()
            })
            .unwrap();
        (repository, credential_key)
    }

    #[tokio::test]
    async fn passes_the_active_source_to_the_read_only_database_reader() {
        let temp = TempDir::new().unwrap();
        let (repository, credential_key) = repository(&temp);
        let credentials = MemoryCredentialStore::default();
        credentials
            .set(&credential_key, SecretString::from("secret"))
            .await
            .unwrap();
        let reader = FakeReader {
            calls: AtomicUsize::new(0),
        };

        let page = TenantCatalogService::new(repository)
            .list(&credentials, &reader, 25)
            .await
            .unwrap();

        assert!(page.entries.is_empty());
        assert_eq!(reader.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn rejects_an_invalid_limit_before_loading_configuration_or_calling_the_reader() {
        let temp = TempDir::new().unwrap();
        let reader = FakeReader {
            calls: AtomicUsize::new(0),
        };

        let error =
            TenantCatalogService::new(ConfigRepository::new(AppPaths::from_root(temp.path())))
                .list(&MemoryCredentialStore::default(), &reader, 0)
                .await
                .unwrap_err();

        assert!(matches!(error, TenantCatalogServiceError::InvalidLimit));
        assert_eq!(reader.calls.load(Ordering::SeqCst), 0);
    }
}
