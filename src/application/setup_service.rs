use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use thiserror::Error;

use crate::{
    domain::{
        ContainerId, ContainerName, CredentialKey, CredentialScope, DatabaseName, MysqlServerUuid,
        MysqlVersion,
    },
    infrastructure::{
        config::{ConfigError, ConfigRepository, LocalTargetConfig, LocalTargetTrust},
        credentials::{CredentialError, CredentialStore},
        mysql::ApprovedMysqlClient,
    },
};

use super::{CredentialProvisionError, persist_config_with_credential};

pub struct NewLocalTargetInput {
    pub docker_context: String,
    pub container_name: ContainerName,
    pub container_id: ContainerId,
    pub username: String,
    pub password: SecretString,
    pub central_database: DatabaseName,
    pub tenant_database_prefix: String,
    pub managed_by_reprodb: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedLocalTarget {
    pub server_version: MysqlVersion,
    pub server_uuid: MysqlServerUuid,
    pub vendor: String,
    pub tls_cipher: Option<String>,
    pub client: ApprovedMysqlClient,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalTargetConfigured {
    pub container_name: ContainerName,
    pub server_version: MysqlVersion,
    pub vendor: String,
    pub docker_context: String,
    pub tenant_database_prefix: String,
    pub managed_by_reprodb: bool,
    pub replaced_existing: bool,
    pub previous_credential_was_missing: bool,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum TargetVerificationError {
    #[error("the selected container is not running; start it and run `reprodb setup` again")]
    ContainerNotRunning,

    #[error("the approved MySQL client could not be prepared")]
    ClientUnavailable,

    #[error("the selected container could not be reached as a MySQL server")]
    ConnectionUnavailable,

    #[error("the selected MySQL target rejected the username or password")]
    AuthenticationFailed,

    #[error("the selected container returned invalid MySQL metadata")]
    InvalidMetadata,

    #[error(
        "the selected MySQL target series {major}.{minor} is not supported by the approved client catalog (currently: {supported})"
    )]
    UnsupportedServerSeries {
        major: u16,
        minor: u16,
        supported: &'static str,
    },
}

#[async_trait]
pub trait LocalTargetVerifier: Send + Sync {
    async fn verify(
        &self,
        input: &NewLocalTargetInput,
    ) -> Result<VerifiedLocalTarget, TargetVerificationError>;
}

#[derive(Debug, Error)]
pub enum SetupServiceError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error(
        "no local MySQL container candidate was found; start one and run `reprodb setup` again"
    )]
    NoCandidates,

    #[error("invalid local target field `{field}`: {reason}")]
    InvalidField {
        field: &'static str,
        reason: &'static str,
    },

    #[error(transparent)]
    Verification(#[from] TargetVerificationError),

    #[error(transparent)]
    Provision(#[from] CredentialProvisionError),

    #[error(
        "the local target was configured, but its previous credential `{orphaned_key}` could not be deleted"
    )]
    PreviousCredentialCleanup {
        orphaned_key: CredentialKey,
        #[source]
        source: CredentialError,
    },
}

pub struct SetupService {
    repository: ConfigRepository,
}

impl SetupService {
    pub fn new(repository: ConfigRepository) -> Self {
        Self { repository }
    }

    pub fn has_local_target(&self) -> Result<bool, SetupServiceError> {
        Ok(self.repository.load()?.local_target.is_some())
    }

    pub fn has_target_named(&self, name: &ContainerName) -> Result<bool, SetupServiceError> {
        Ok(self.repository.load()?.local_target_named(name).is_some())
    }

    pub async fn configure(
        &self,
        store: &dyn CredentialStore,
        verifier: &dyn LocalTargetVerifier,
        input: NewLocalTargetInput,
    ) -> Result<LocalTargetConfigured, SetupServiceError> {
        validate_input(&input)?;
        let mut config = self.repository.load()?;
        if let Some(context) = &config.client_runtime.docker_context
            && context != &input.docker_context
        {
            return Err(SetupServiceError::InvalidField {
                field: "docker_context",
                reason: "differs from the context already used by source profiles",
            });
        }

        let verified = verifier.verify(&input).await?;
        let detected_series = format!(
            "{}.{}",
            verified.server_version.major, verified.server_version.minor
        );
        if detected_series != verified.client.series() {
            return Err(TargetVerificationError::InvalidMetadata.into());
        }

        let previous_credential = config
            .local_target_named(&input.container_name)
            .map(|target| target.credential_key);
        let credential_key = CredentialKey::new(CredentialScope::Target);
        config.client_runtime.docker_context = Some(input.docker_context.clone());

        if let Some(previous_default) = config.local_target.take()
            && previous_default.container_name != input.container_name
        {
            config
                .local_targets
                .insert(previous_default.container_name.clone(), previous_default);
        }
        config.local_targets.remove(&input.container_name);
        config.local_target = Some(LocalTargetConfig {
            docker_context: input.docker_context.clone(),
            container_name: input.container_name.clone(),
            container_id: input.container_id,
            username: input.username,
            credential_key,
            central_database: input.central_database,
            trust: if input.managed_by_reprodb {
                LocalTargetTrust::ReprodbManaged
            } else {
                LocalTargetTrust::UserConfirmed
            },
            tenant_database_prefix: input.tenant_database_prefix.clone(),
        });

        persist_config_with_credential(
            store,
            &self.repository,
            credential_key,
            input.password,
            &config,
        )
        .await?;

        let mut previous_credential_was_missing = false;
        if let Some(previous) = previous_credential {
            match store.delete(&previous).await {
                Ok(()) => {}
                Err(CredentialError::NotFound) => previous_credential_was_missing = true,
                Err(source) => {
                    return Err(SetupServiceError::PreviousCredentialCleanup {
                        orphaned_key: previous,
                        source,
                    });
                }
            }
        }

        Ok(LocalTargetConfigured {
            container_name: input.container_name,
            server_version: verified.server_version,
            vendor: verified.vendor,
            docker_context: input.docker_context,
            tenant_database_prefix: input.tenant_database_prefix,
            managed_by_reprodb: input.managed_by_reprodb,
            replaced_existing: previous_credential.is_some(),
            previous_credential_was_missing,
        })
    }
}

fn validate_input(input: &NewLocalTargetInput) -> Result<(), SetupServiceError> {
    if input.username.is_empty()
        || input.username.trim() != input.username
        || input.username.chars().count() > 32
        || input.username.chars().any(char::is_control)
    {
        return Err(SetupServiceError::InvalidField {
            field: "username",
            reason: "must contain 1-32 characters without surrounding whitespace",
        });
    }
    if input.password.expose_secret().is_empty() || input.password.expose_secret().contains('\0') {
        return Err(SetupServiceError::InvalidField {
            field: "password",
            reason: "cannot be empty or contain NUL",
        });
    }
    if input.tenant_database_prefix.is_empty()
        || input.tenant_database_prefix.len() > 32
        || !input
            .tenant_database_prefix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(SetupServiceError::InvalidField {
            field: "tenant_database_prefix",
            reason: "must contain 1-32 ASCII letters, digits or `_`",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use tempfile::TempDir;

    use super::*;
    use crate::{
        domain::{CredentialScope, MysqlTlsMode, ProfileName},
        infrastructure::{
            config::{
                AppConfig, AppPaths, MysqlClientConfig, MysqlFamily, SourceProfileConfig,
                TenantResolverConfig,
            },
            credentials::MemoryCredentialStore,
            mysql::ClientCatalog,
        },
    };

    fn repository(temp: &TempDir) -> ConfigRepository {
        ConfigRepository::new(AppPaths::new(
            temp.path().join("config"),
            temp.path().join("cache"),
            temp.path().join("data"),
        ))
    }

    fn input() -> NewLocalTargetInput {
        NewLocalTargetInput {
            docker_context: "desktop-linux".to_owned(),
            container_name: ContainerName::try_from("mysql-8").unwrap(),
            container_id: ContainerId::try_from("a".repeat(64)).unwrap(),
            username: "root".to_owned(),
            password: SecretString::from("password-that-must-not-leak"),
            central_database: DatabaseName::try_from("salt_central").unwrap(),
            tenant_database_prefix: crate::infrastructure::config::DEFAULT_TENANT_DATABASE_PREFIX
                .to_owned(),
            managed_by_reprodb: false,
        }
    }

    struct FakeVerifier {
        calls: AtomicUsize,
        result: Result<VerifiedLocalTarget, TargetVerificationError>,
    }

    impl FakeVerifier {
        fn successful() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                result: Ok(VerifiedLocalTarget {
                    server_version: "8.4.4".parse().unwrap(),
                    server_uuid: "22222222-2222-4222-8222-222222222222".parse().unwrap(),
                    vendor: "MySQL Community Server".to_owned(),
                    tls_cipher: Some("TLS_AES_256_GCM_SHA384".to_owned()),
                    client: ClientCatalog::resolve("8.4").unwrap(),
                }),
            }
        }
    }

    #[async_trait]
    impl LocalTargetVerifier for FakeVerifier {
        async fn verify(
            &self,
            _input: &NewLocalTargetInput,
        ) -> Result<VerifiedLocalTarget, TargetVerificationError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result.clone()
        }
    }

    #[tokio::test]
    async fn verifies_and_persists_the_local_target_and_credential() {
        let temp = TempDir::new().unwrap();
        let repository = repository(&temp);
        let store = MemoryCredentialStore::default();
        let verifier = FakeVerifier::successful();

        let configured = SetupService::new(repository.clone())
            .configure(&store, &verifier, input())
            .await
            .unwrap();

        assert_eq!(configured.container_name.as_str(), "mysql-8");
        assert!(!configured.replaced_existing);
        assert_eq!(verifier.calls.load(Ordering::SeqCst), 1);
        let config = repository.load().unwrap();
        let target = config.local_target.unwrap();
        assert_eq!(target.container_id.as_str(), "a".repeat(64));
        assert_eq!(target.trust, LocalTargetTrust::UserConfirmed);
        assert_eq!(target.tenant_database_prefix, "salt_");
        assert_eq!(
            config.client_runtime.docker_context.as_deref(),
            Some("desktop-linux")
        );
        assert_eq!(
            store
                .get(&target.credential_key)
                .await
                .unwrap()
                .expose_secret(),
            "password-that-must-not-leak"
        );
    }

    #[tokio::test]
    async fn invalid_database_prefix_is_rejected_before_connection_or_credentials() {
        let temp = TempDir::new().unwrap();
        let repository = repository(&temp);
        let store = MemoryCredentialStore::default();
        let verifier = FakeVerifier::successful();
        let mut input = input();
        input.tenant_database_prefix = "salt_; DROP".to_owned();

        let error = SetupService::new(repository.clone())
            .configure(&store, &verifier, input)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            SetupServiceError::InvalidField {
                field: "tenant_database_prefix",
                ..
            }
        ));
        assert_eq!(verifier.calls.load(Ordering::SeqCst), 0);
        assert!(repository.load().unwrap().local_target.is_none());
    }

    #[tokio::test]
    async fn context_mismatch_is_rejected_before_connection_and_credentials() {
        let temp = TempDir::new().unwrap();
        let repository = repository(&temp);
        let client = ClientCatalog::resolve("8.4").unwrap();
        let profile_name = ProfileName::try_from("source").unwrap();
        repository
            .save(&AppConfig {
                client_runtime: crate::infrastructure::config::ClientRuntimeConfig {
                    docker_context: Some("default".to_owned()),
                    ..Default::default()
                },
                profiles: BTreeMap::from([(
                    profile_name.clone(),
                    SourceProfileConfig {
                        host: "127.0.0.1".to_owned(),
                        port: 3306,
                        username: "root".to_owned(),
                        credential_key: CredentialKey::new(CredentialScope::Source),
                        mysql_family: MysqlFamily::Mysql,
                        mysql_series: "8.4".to_owned(),
                        production: false,
                        tls_mode: MysqlTlsMode::Preferred,
                        tls_material: Default::default(),
                        client: MysqlClientConfig {
                            image: client.image().to_owned(),
                        },
                        tenant_resolver: TenantResolverConfig::SaltCentral {
                            central_database: DatabaseName::try_from("salt_central").unwrap(),
                            allow_domain_lookup: true,
                        },
                    },
                )]),
                active_profile: Some(profile_name),
                ..Default::default()
            })
            .unwrap();
        let verifier = FakeVerifier::successful();

        let error = SetupService::new(repository)
            .configure(&MemoryCredentialStore::default(), &verifier, input())
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            SetupServiceError::InvalidField {
                field: "docker_context",
                ..
            }
        ));
        assert_eq!(verifier.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn replacing_a_target_removes_its_previous_credential_after_commit() {
        let temp = TempDir::new().unwrap();
        let repository = repository(&temp);
        let store = MemoryCredentialStore::default();
        let verifier = FakeVerifier::successful();
        let service = SetupService::new(repository.clone());

        service.configure(&store, &verifier, input()).await.unwrap();
        let previous_key = repository
            .load()
            .unwrap()
            .local_target
            .unwrap()
            .credential_key;

        let configured = service.configure(&store, &verifier, input()).await.unwrap();

        assert!(configured.replaced_existing);
        assert!(!configured.previous_credential_was_missing);
        assert!(matches!(
            store.get(&previous_key).await,
            Err(CredentialError::NotFound)
        ));
        let current_key = repository
            .load()
            .unwrap()
            .local_target
            .unwrap()
            .credential_key;
        assert!(store.get(&current_key).await.is_ok());
    }

    #[tokio::test]
    async fn adding_another_target_preserves_the_previous_target_and_credential() {
        let temp = TempDir::new().unwrap();
        let repository = repository(&temp);
        let store = MemoryCredentialStore::default();
        let verifier = FakeVerifier::successful();
        let service = SetupService::new(repository.clone());

        service.configure(&store, &verifier, input()).await.unwrap();
        let previous_key = repository
            .load()
            .unwrap()
            .local_target
            .unwrap()
            .credential_key;
        let mut second = input();
        second.container_name = ContainerName::try_from("mysql-target").unwrap();
        second.container_id = ContainerId::try_from("b".repeat(64)).unwrap();
        second.password = SecretString::from("another-local-password");

        let configured = service.configure(&store, &verifier, second).await.unwrap();
        let config = repository.load().unwrap();

        assert!(!configured.replaced_existing);
        assert_eq!(
            config
                .local_target
                .as_ref()
                .unwrap()
                .container_name
                .as_str(),
            "mysql-target"
        );
        assert_eq!(
            config
                .local_targets
                .get(&ContainerName::try_from("mysql-8").unwrap())
                .unwrap()
                .credential_key,
            previous_key
        );
        assert!(store.get(&previous_key).await.is_ok());
        assert!(
            store
                .get(&config.local_target.unwrap().credential_key)
                .await
                .is_ok()
        );
    }
}
