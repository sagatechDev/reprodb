use async_trait::async_trait;
use secrecy::SecretString;
use thiserror::Error;

use crate::{
    domain::{ContainerId, ContainerName, DatabaseName, MysqlVersion},
    infrastructure::{
        config::{ConfigError, ConfigRepository, LocalTargetConfig, LocalTargetTrust},
        credentials::{CredentialError, CredentialStore},
        mysql::ApprovedMysqlClient,
    },
};

pub struct LocalTargetAttestationRequest<'a> {
    pub configured: &'a LocalTargetConfig,
    pub password: &'a SecretString,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalTargetAttestation {
    pub docker_context: String,
    pub container_name: ContainerName,
    pub container_id: ContainerId,
    pub managed_by_reprodb: bool,
    pub server_version: MysqlVersion,
    pub vendor: String,
    pub client: ApprovedMysqlClient,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum LocalTargetAttestationError {
    #[error("Docker is unavailable while validating the local restore target")]
    DockerUnavailable,

    #[error("the configured Docker context is not a local Unix-socket context")]
    RemoteContext,

    #[error("the active local Docker context differs from the configured restore target")]
    ContextChanged,

    #[error("the configured local restore container was not found by exact ID and name")]
    ContainerNotFound,

    #[error("the configured local restore container is not running")]
    ContainerNotRunning,

    #[error("the approved MySQL client is unavailable for target validation")]
    ClientUnavailable,

    #[error("the local restore target rejected its stored credential")]
    AuthenticationFailed,

    #[error("the local restore target could not be reached")]
    ConnectionUnavailable,

    #[error("Docker or MySQL returned invalid target identity metadata")]
    InvalidMetadata,
}

#[async_trait]
pub trait LocalTargetAttestor: Send + Sync {
    async fn attest(
        &self,
        request: LocalTargetAttestationRequest<'_>,
    ) -> Result<LocalTargetAttestation, LocalTargetAttestationError>;
}

pub struct LocalTargetGate {
    repository: ConfigRepository,
}

impl LocalTargetGate {
    pub fn new(repository: ConfigRepository) -> Self {
        Self { repository }
    }

    pub async fn verify(
        &self,
        credentials: &dyn CredentialStore,
        attestor: &dyn LocalTargetAttestor,
    ) -> Result<GuardedLocalTarget, LocalTargetGateError> {
        let config = self.repository.load()?;
        let configured = config
            .local_target
            .as_ref()
            .ok_or(LocalTargetGateError::NotConfigured)?;
        let runtime_context = config
            .client_runtime
            .docker_context
            .as_deref()
            .ok_or(LocalTargetGateError::RuntimeContextMissing)?;
        if runtime_context != configured.docker_context {
            return Err(LocalTargetGateError::ContextIdentityChanged);
        }

        let password = credentials.get(&configured.credential_key).await?;
        let attested = attestor
            .attest(LocalTargetAttestationRequest {
                configured,
                password: &password,
            })
            .await?;

        if attested.docker_context != configured.docker_context {
            return Err(LocalTargetGateError::ContextIdentityChanged);
        }
        if attested.container_id != configured.container_id
            || attested.container_name != configured.container_name
        {
            return Err(LocalTargetGateError::ContainerIdentityChanged);
        }
        if configured.trust == LocalTargetTrust::ReprodbManaged && !attested.managed_by_reprodb {
            return Err(LocalTargetGateError::ManagedMarkerMissing);
        }
        if !attested.vendor.to_ascii_lowercase().contains("mysql") {
            return Err(LocalTargetGateError::UnsupportedVendor);
        }
        let detected_series = format!(
            "{}.{}",
            attested.server_version.major, attested.server_version.minor
        );
        if detected_series != attested.client.series() {
            return Err(LocalTargetGateError::UnsupportedServerSeries);
        }

        Ok(GuardedLocalTarget {
            docker_context: configured.docker_context.clone(),
            container_name: configured.container_name.clone(),
            container_id: configured.container_id.clone(),
            username: configured.username.clone(),
            password,
            central_database: configured.central_database.clone(),
            tenant_database_prefix: configured.tenant_database_prefix.clone(),
            server_version: attested.server_version,
            vendor: attested.vendor,
            client: attested.client,
        })
    }
}

pub struct GuardedLocalTarget {
    docker_context: String,
    container_name: ContainerName,
    container_id: ContainerId,
    username: String,
    password: SecretString,
    central_database: DatabaseName,
    tenant_database_prefix: String,
    server_version: MysqlVersion,
    vendor: String,
    client: ApprovedMysqlClient,
}

impl std::fmt::Debug for GuardedLocalTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardedLocalTarget")
            .field("docker_context", &self.docker_context)
            .field("container_name", &self.container_name)
            .field("container_id", &self.container_id)
            .field("username", &self.username)
            .field("central_database", &self.central_database)
            .field("tenant_database_prefix", &self.tenant_database_prefix)
            .field("server_version", &self.server_version)
            .field("vendor", &self.vendor)
            .field("client", &self.client)
            .finish_non_exhaustive()
    }
}

impl GuardedLocalTarget {
    pub fn container_name(&self) -> &ContainerName {
        &self.container_name
    }

    pub const fn server_version(&self) -> MysqlVersion {
        self.server_version
    }

    pub fn vendor(&self) -> &str {
        &self.vendor
    }

    pub fn authorize_tenant_database(
        self,
        database: DatabaseName,
    ) -> Result<AuthorizedLocalTarget, LocalTargetGateError> {
        if database == self.central_database
            || !database.as_str().starts_with(&self.tenant_database_prefix)
        {
            return Err(LocalTargetGateError::DatabaseOutsideAllowlist);
        }
        Ok(AuthorizedLocalTarget {
            target: self,
            database,
        })
    }
}

pub struct AuthorizedLocalTarget {
    target: GuardedLocalTarget,
    database: DatabaseName,
}

impl std::fmt::Debug for AuthorizedLocalTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthorizedLocalTarget")
            .field("target", &self.target)
            .field("database", &self.database)
            .finish()
    }
}

impl AuthorizedLocalTarget {
    pub fn container_name(&self) -> &ContainerName {
        &self.target.container_name
    }

    pub fn database(&self) -> &DatabaseName {
        &self.database
    }

    pub const fn server_version(&self) -> MysqlVersion {
        self.target.server_version
    }

    pub fn docker_context(&self) -> &str {
        &self.target.docker_context
    }

    pub fn container_id(&self) -> &ContainerId {
        &self.target.container_id
    }

    pub fn username(&self) -> &str {
        &self.target.username
    }

    pub fn password(&self) -> &SecretString {
        &self.target.password
    }

    pub const fn client(&self) -> ApprovedMysqlClient {
        self.target.client
    }

    #[cfg(test)]
    pub(crate) fn for_test(database: DatabaseName) -> Self {
        Self {
            target: GuardedLocalTarget {
                docker_context: "desktop-linux".to_owned(),
                container_name: ContainerName::try_from("mysql-8").unwrap(),
                container_id: ContainerId::try_from("a".repeat(64)).unwrap(),
                username: "root".to_owned(),
                password: SecretString::from("local-test-password"),
                central_database: DatabaseName::try_from("salt_central").unwrap(),
                tenant_database_prefix: "salt_".to_owned(),
                server_version: "8.4.4".parse().unwrap(),
                vendor: "MySQL Community Server - GPL".to_owned(),
                client: crate::infrastructure::mysql::ClientCatalog::resolve("8.4").unwrap(),
            },
            database,
        }
    }
}

#[derive(Debug, Error)]
pub enum LocalTargetGateError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error("no local restore target is configured; run `reprodb setup`")]
    NotConfigured,

    #[error("the MySQL client Docker context is missing; run `reprodb setup`")]
    RuntimeContextMissing,

    #[error(transparent)]
    Credential(#[from] CredentialError),

    #[error(transparent)]
    Attestation(#[from] LocalTargetAttestationError),

    #[error("the active Docker context no longer matches the configured local target")]
    ContextIdentityChanged,

    #[error("the local restore container identity changed; run `reprodb setup` again")]
    ContainerIdentityChanged,

    #[error("the reprodb-managed target lost its ownership label; run `reprodb setup` again")]
    ManagedMarkerMissing,

    #[error("the local restore target is not an approved MySQL server")]
    UnsupportedVendor,

    #[error("the local restore target version is incompatible with the approved MySQL client")]
    UnsupportedServerSeries,

    #[error("the database is outside the local target tenant allowlist")]
    DatabaseOutsideAllowlist,
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use secrecy::SecretString;
    use tempfile::TempDir;

    use crate::{
        domain::CredentialKey,
        infrastructure::{
            config::{
                AppConfig, AppPaths, ClientRuntimeConfig, DEFAULT_TENANT_DATABASE_PREFIX,
                LocalTargetTrust,
            },
            credentials::MemoryCredentialStore,
            mysql::ClientCatalog,
        },
    };

    use super::*;

    struct FakeAttestor {
        calls: AtomicUsize,
        result: Result<LocalTargetAttestation, LocalTargetAttestationError>,
    }

    #[async_trait]
    impl LocalTargetAttestor for FakeAttestor {
        async fn attest(
            &self,
            _request: LocalTargetAttestationRequest<'_>,
        ) -> Result<LocalTargetAttestation, LocalTargetAttestationError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result.clone()
        }
    }

    fn target_key() -> CredentialKey {
        "target:550e8400-e29b-41d4-a716-446655440001"
            .parse()
            .unwrap()
    }

    fn configured_target(trust: LocalTargetTrust) -> LocalTargetConfig {
        LocalTargetConfig {
            docker_context: "desktop-linux".to_owned(),
            container_name: ContainerName::try_from("mysql-8").unwrap(),
            container_id: ContainerId::try_from("a".repeat(64)).unwrap(),
            username: "root".to_owned(),
            credential_key: target_key(),
            central_database: DatabaseName::try_from("salt_central").unwrap(),
            trust,
            tenant_database_prefix: DEFAULT_TENANT_DATABASE_PREFIX.to_owned(),
        }
    }

    fn repository(temp: &TempDir, trust: LocalTargetTrust) -> ConfigRepository {
        let repository = ConfigRepository::new(AppPaths::new(
            temp.path().join("config"),
            temp.path().join("cache"),
            temp.path().join("data"),
        ));
        repository
            .save(&AppConfig {
                client_runtime: ClientRuntimeConfig {
                    docker_context: Some("desktop-linux".to_owned()),
                    ..ClientRuntimeConfig::default()
                },
                local_target: Some(configured_target(trust)),
                profiles: BTreeMap::new(),
                ..AppConfig::default()
            })
            .unwrap();
        repository
    }

    fn attestation(managed_by_reprodb: bool) -> LocalTargetAttestation {
        LocalTargetAttestation {
            docker_context: "desktop-linux".to_owned(),
            container_name: ContainerName::try_from("mysql-8").unwrap(),
            container_id: ContainerId::try_from("a".repeat(64)).unwrap(),
            managed_by_reprodb,
            server_version: "8.4.4".parse().unwrap(),
            vendor: "MySQL Community Server - GPL".to_owned(),
            client: ClientCatalog::resolve("8.4").unwrap(),
        }
    }

    async fn credentials() -> MemoryCredentialStore {
        let credentials = MemoryCredentialStore::default();
        credentials
            .set(
                &target_key(),
                SecretString::from("password-that-must-not-leak"),
            )
            .await
            .unwrap();
        credentials
    }

    #[tokio::test]
    async fn produces_an_authorized_target_only_after_full_attestation() {
        let temp = TempDir::new().unwrap();
        let attestor = FakeAttestor {
            calls: AtomicUsize::new(0),
            result: Ok(attestation(false)),
        };
        let guarded = LocalTargetGate::new(repository(&temp, LocalTargetTrust::UserConfirmed))
            .verify(&credentials().await, &attestor)
            .await
            .unwrap();

        let authorized = guarded
            .authorize_tenant_database(DatabaseName::try_from("salt_polymer").unwrap())
            .unwrap();

        assert_eq!(authorized.container_name().as_str(), "mysql-8");
        assert_eq!(authorized.database().as_str(), "salt_polymer");
        assert_eq!(authorized.server_version().to_string(), "8.4.4");
        assert_eq!(attestor.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn rejects_central_and_out_of_namespace_databases() {
        for database in ["salt_central", "customer_data"] {
            let temp = TempDir::new().unwrap();
            let guarded = LocalTargetGate::new(repository(&temp, LocalTargetTrust::UserConfirmed))
                .verify(
                    &credentials().await,
                    &FakeAttestor {
                        calls: AtomicUsize::new(0),
                        result: Ok(attestation(false)),
                    },
                )
                .await
                .unwrap();

            assert!(matches!(
                guarded.authorize_tenant_database(DatabaseName::try_from(database).unwrap()),
                Err(LocalTargetGateError::DatabaseOutsideAllowlist)
            ));
        }
    }

    #[tokio::test]
    async fn rejects_changed_context_container_and_managed_label() {
        let cases = [
            (
                LocalTargetTrust::UserConfirmed,
                LocalTargetAttestation {
                    docker_context: "default".to_owned(),
                    ..attestation(false)
                },
                "context",
            ),
            (
                LocalTargetTrust::UserConfirmed,
                LocalTargetAttestation {
                    container_id: ContainerId::try_from("b".repeat(64)).unwrap(),
                    ..attestation(false)
                },
                "container",
            ),
            (
                LocalTargetTrust::ReprodbManaged,
                attestation(false),
                "managed",
            ),
        ];

        for (trust, evidence, expected) in cases {
            let temp = TempDir::new().unwrap();
            let error = LocalTargetGate::new(repository(&temp, trust))
                .verify(
                    &credentials().await,
                    &FakeAttestor {
                        calls: AtomicUsize::new(0),
                        result: Ok(evidence),
                    },
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains(expected));
        }
    }

    #[tokio::test]
    async fn missing_credential_stops_before_external_attestation() {
        let temp = TempDir::new().unwrap();
        let attestor = FakeAttestor {
            calls: AtomicUsize::new(0),
            result: Ok(attestation(false)),
        };

        let error = LocalTargetGate::new(repository(&temp, LocalTargetTrust::UserConfirmed))
            .verify(&MemoryCredentialStore::default(), &attestor)
            .await
            .unwrap_err();

        assert!(matches!(error, LocalTargetGateError::Credential(_)));
        assert_eq!(attestor.calls.load(Ordering::SeqCst), 0);
        assert!(!error.to_string().contains("password-that-must-not-leak"));
    }

    #[tokio::test]
    async fn absence_of_setup_is_rejected_before_credentials_or_docker() {
        let temp = TempDir::new().unwrap();
        let repository = ConfigRepository::new(AppPaths::new(
            temp.path().join("config"),
            temp.path().join("cache"),
            temp.path().join("data"),
        ));
        let attestor = FakeAttestor {
            calls: AtomicUsize::new(0),
            result: Ok(attestation(false)),
        };

        let error = LocalTargetGate::new(repository)
            .verify(&MemoryCredentialStore::default(), &attestor)
            .await
            .unwrap_err();

        assert!(matches!(error, LocalTargetGateError::NotConfigured));
        assert_eq!(attestor.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn incompatible_vendor_or_series_cannot_produce_a_guarded_target() {
        let cases = [
            LocalTargetAttestation {
                vendor: "MariaDB Server".to_owned(),
                ..attestation(false)
            },
            LocalTargetAttestation {
                server_version: "8.0.45".parse().unwrap(),
                ..attestation(false)
            },
        ];

        for evidence in cases {
            let temp = TempDir::new().unwrap();
            let error = LocalTargetGate::new(repository(&temp, LocalTargetTrust::UserConfirmed))
                .verify(
                    &credentials().await,
                    &FakeAttestor {
                        calls: AtomicUsize::new(0),
                        result: Ok(evidence),
                    },
                )
                .await
                .unwrap_err();

            assert!(matches!(
                error,
                LocalTargetGateError::UnsupportedVendor
                    | LocalTargetGateError::UnsupportedServerSeries
            ));
        }
    }
}
