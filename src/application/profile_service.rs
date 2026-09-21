use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use thiserror::Error;

use crate::{
    domain::{
        CredentialKey, CredentialScope, MysqlTlsMaterialPaths, MysqlTlsMode, MysqlVersion,
        ProfileName,
    },
    infrastructure::{
        config::{
            ConfigError, ConfigRepository, MysqlClientConfig, MysqlFamily, SourceProfileConfig,
        },
        credentials::{CredentialError, CredentialStore},
        mysql::ApprovedMysqlClient,
    },
};

use super::credential_transaction::{CredentialProvisionError, persist_config_with_credential};

pub struct NewProfileInput {
    pub name: ProfileName,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: SecretString,
    pub tls_mode: MysqlTlsMode,
    pub tls_material: MysqlTlsMaterialPaths,
    pub production: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedSource {
    pub docker_context: String,
    pub server_version: MysqlVersion,
    pub vendor: String,
    pub tls_cipher: Option<String>,
    pub client: ApprovedMysqlClient,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileCreated {
    pub name: ProfileName,
    pub docker_context: String,
    pub server_version: MysqlVersion,
    pub vendor: String,
    pub client_version: MysqlVersion,
    pub tls_mode: MysqlTlsMode,
    pub production: bool,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SourceVerificationError {
    #[error("Docker is unavailable; start Docker and verify its current context")]
    DockerUnavailable,

    #[error("the approved MySQL client could not be prepared")]
    ClientUnavailable,

    #[error("the MySQL source could not be reached; verify host, port, VPN and Docker networking")]
    NetworkUnavailable,

    #[error("the MySQL source rejected the username or password")]
    AuthenticationFailed,

    #[error("the MySQL source returned invalid connection metadata")]
    InvalidMetadata,

    #[error("the MySQL source did not negotiate the required TLS transport")]
    TlsRequiredButNotNegotiated,

    #[error(
        "the detected MySQL server series {major}.{minor} is not supported by the approved client catalog (currently: {supported})"
    )]
    UnsupportedServerSeries {
        major: u16,
        minor: u16,
        supported: &'static str,
    },
}

#[async_trait]
pub trait SourceProfileVerifier: Send + Sync {
    async fn verify(
        &self,
        input: &NewProfileInput,
    ) -> Result<VerifiedSource, SourceVerificationError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileSummary {
    pub name: ProfileName,
    pub active: bool,
    pub host: String,
    pub port: u16,
    pub mysql_series: String,
    pub tls_mode: MysqlTlsMode,
    pub production: bool,
}

#[derive(Debug, Error)]
pub enum ProfileServiceError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error("source profile does not exist; run `reprodb profile list`")]
    NotFound,

    #[error("a source profile with this name already exists")]
    AlreadyExists,

    #[error("invalid source profile field `{field}`: {reason}")]
    InvalidField {
        field: &'static str,
        reason: &'static str,
    },

    #[error(transparent)]
    Verification(#[from] SourceVerificationError),

    #[error(transparent)]
    Provision(#[from] CredentialProvisionError),

    #[error(
        "profile was removed, but credential `{orphaned_key}` could not be deleted; retry cleanup from the local credential store"
    )]
    CredentialCleanup {
        orphaned_key: CredentialKey,
        #[source]
        source: CredentialError,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProfileRemoval {
    pub was_active: bool,
    pub credential_was_missing: bool,
}

pub struct ProfileService {
    repository: ConfigRepository,
}

impl ProfileService {
    pub fn new(repository: ConfigRepository) -> Self {
        Self { repository }
    }

    pub fn list(&self) -> Result<Vec<ProfileSummary>, ProfileServiceError> {
        let config = self.repository.load()?;
        Ok(config
            .profiles
            .into_iter()
            .map(|(name, profile)| ProfileSummary {
                active: config.active_profile.as_ref() == Some(&name),
                name,
                host: profile.host,
                port: profile.port,
                mysql_series: profile.mysql_series,
                tls_mode: profile.tls_mode,
                production: profile.production,
            })
            .collect())
    }

    pub fn ensure_name_available(&self, name: &ProfileName) -> Result<(), ProfileServiceError> {
        if self.repository.load()?.profiles.contains_key(name) {
            return Err(ProfileServiceError::AlreadyExists);
        }
        Ok(())
    }

    pub async fn add(
        &self,
        store: &dyn CredentialStore,
        verifier: &dyn SourceProfileVerifier,
        input: NewProfileInput,
    ) -> Result<ProfileCreated, ProfileServiceError> {
        let mut config = self.repository.load()?;
        if config.profiles.contains_key(&input.name) {
            return Err(ProfileServiceError::AlreadyExists);
        }
        validate_new_profile(&input)?;

        let verified = verifier.verify(&input).await?;
        if input.tls_mode.requires_encrypted_transport() && verified.tls_cipher.is_none() {
            return Err(SourceVerificationError::TlsRequiredButNotNegotiated.into());
        }
        let detected_series = format!(
            "{}.{}",
            verified.server_version.major, verified.server_version.minor
        );
        if detected_series != verified.client.series() {
            return Err(SourceVerificationError::InvalidMetadata.into());
        }

        if let Some(configured_context) = &config.client_runtime.docker_context
            && configured_context != &verified.docker_context
        {
            return Err(ProfileServiceError::InvalidField {
                field: "client_runtime.docker_context",
                reason: "differs from the current Docker context",
            });
        }
        config.client_runtime.docker_context = Some(verified.docker_context.clone());

        let credential_key = CredentialKey::new(CredentialScope::Source);
        config.profiles.insert(
            input.name.clone(),
            SourceProfileConfig {
                host: input.host,
                port: input.port,
                username: input.username,
                credential_key,
                mysql_family: MysqlFamily::Mysql,
                mysql_series: detected_series,
                production: input.production,
                tls_mode: input.tls_mode,
                tls_material: input.tls_material,
                client: MysqlClientConfig {
                    image: verified.client.image().to_owned(),
                },
            },
        );
        config.active_profile = Some(input.name.clone());

        persist_config_with_credential(
            store,
            &self.repository,
            credential_key,
            input.password,
            &config,
        )
        .await?;

        Ok(ProfileCreated {
            name: input.name,
            docker_context: verified.docker_context,
            server_version: verified.server_version,
            vendor: verified.vendor,
            client_version: verified.client.version(),
            tls_mode: input.tls_mode,
            production: input.production,
        })
    }

    pub fn activate(&self, name: &ProfileName) -> Result<(), ProfileServiceError> {
        let mut config = self.repository.load()?;
        if !config.profiles.contains_key(name) {
            return Err(ProfileServiceError::NotFound);
        }
        config.active_profile = Some(name.clone());
        self.repository.save(&config)?;
        Ok(())
    }

    pub async fn remove(
        &self,
        store: &dyn CredentialStore,
        name: &ProfileName,
    ) -> Result<ProfileRemoval, ProfileServiceError> {
        let mut config = self.repository.load()?;
        let profile = config
            .profiles
            .remove(name)
            .ok_or(ProfileServiceError::NotFound)?;
        let was_active = config.active_profile.as_ref() == Some(name);
        if was_active {
            config.active_profile = None;
        }

        self.repository.save(&config)?;

        let credential_was_missing = match store.delete(&profile.credential_key).await {
            Ok(()) => false,
            Err(CredentialError::NotFound) => true,
            Err(source) => {
                return Err(ProfileServiceError::CredentialCleanup {
                    orphaned_key: profile.credential_key,
                    source,
                });
            }
        };

        Ok(ProfileRemoval {
            was_active,
            credential_was_missing,
        })
    }
}

fn validate_new_profile(input: &NewProfileInput) -> Result<(), ProfileServiceError> {
    validate_input_text("host", &input.host, 255, false)?;
    validate_input_text("username", &input.username, 32, true)?;
    if input.port == 0 {
        return Err(ProfileServiceError::InvalidField {
            field: "port",
            reason: "must be between 1 and 65535",
        });
    }
    if input.password.expose_secret().is_empty() || input.password.expose_secret().contains('\0') {
        return Err(ProfileServiceError::InvalidField {
            field: "password",
            reason: "cannot be empty or contain NUL",
        });
    }
    validate_tls_material(input)?;
    if input.production && input.tls_mode != MysqlTlsMode::VerifyIdentity {
        return Err(ProfileServiceError::InvalidField {
            field: "tls_mode",
            reason: "must verify CA and hostname for a production source",
        });
    }
    Ok(())
}

fn validate_tls_material(input: &NewProfileInput) -> Result<(), ProfileServiceError> {
    let material = &input.tls_material;
    if input.tls_mode.verifies_certificate_authority() && material.ca.is_none() {
        return Err(ProfileServiceError::InvalidField {
            field: "tls_ca",
            reason: "is required for verify-ca or verify-identity",
        });
    }
    if material.cert.is_some() != material.key.is_some() {
        return Err(ProfileServiceError::InvalidField {
            field: "tls_cert/tls_key",
            reason: "must be configured together",
        });
    }
    if input.tls_mode == MysqlTlsMode::Disabled && !material.is_empty() {
        return Err(ProfileServiceError::InvalidField {
            field: "tls_material",
            reason: "cannot be used when TLS is disabled",
        });
    }
    for path in [&material.ca, &material.cert, &material.key]
        .into_iter()
        .flatten()
    {
        if !path.is_absolute() {
            return Err(ProfileServiceError::InvalidField {
                field: "tls_material",
                reason: "paths must be absolute",
            });
        }
    }
    Ok(())
}

fn validate_input_text(
    field: &'static str,
    value: &str,
    max_chars: usize,
    allow_internal_whitespace: bool,
) -> Result<(), ProfileServiceError> {
    let invalid_whitespace = if allow_internal_whitespace {
        value.trim() != value
    } else {
        value.chars().any(char::is_whitespace)
    };
    if value.is_empty()
        || value.chars().count() > max_chars
        || value.chars().any(char::is_control)
        || invalid_whitespace
    {
        return Err(ProfileServiceError::InvalidField {
            field,
            reason: "has an invalid format or length",
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
        domain::{CredentialScope, MysqlTlsMode},
        infrastructure::{
            config::{AppConfig, AppPaths, MysqlClientConfig, MysqlFamily, SourceProfileConfig},
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

    fn profile(host: &str, production: bool) -> SourceProfileConfig {
        let client = ClientCatalog::resolve("8.4").unwrap();
        SourceProfileConfig {
            host: host.to_owned(),
            port: 3306,
            username: "readonly_user".to_owned(),
            credential_key: crate::domain::CredentialKey::new(CredentialScope::Source),
            mysql_family: MysqlFamily::Mysql,
            mysql_series: "8.4".to_owned(),
            production,
            tls_mode: if production {
                MysqlTlsMode::VerifyIdentity
            } else {
                MysqlTlsMode::Preferred
            },
            tls_material: if production {
                MysqlTlsMaterialPaths {
                    ca: Some(std::path::PathBuf::from("/tmp/reprodb-test-ca.pem")),
                    ..Default::default()
                }
            } else {
                Default::default()
            },
            client: MysqlClientConfig {
                image: client.image().to_owned(),
            },
        }
    }

    fn save_profiles(repository: &ConfigRepository) {
        let local = ProfileName::try_from("local").unwrap();
        let production = ProfileName::try_from("production").unwrap();
        let config = AppConfig {
            active_profile: Some(local.clone()),
            profiles: BTreeMap::from([
                (local, profile("127.0.0.1", false)),
                (production, profile("mysql.salt.internal", true)),
            ]),
            ..AppConfig::default()
        };
        repository.save(&config).unwrap();
    }

    fn new_profile_input(name: &str) -> NewProfileInput {
        NewProfileInput {
            name: ProfileName::try_from(name).unwrap(),
            host: "127.0.0.1".to_owned(),
            port: 3306,
            username: "root".to_owned(),
            password: SecretString::from("password-that-must-not-leak"),
            tls_mode: MysqlTlsMode::Preferred,
            tls_material: Default::default(),
            production: false,
        }
    }

    struct FakeVerifier {
        calls: AtomicUsize,
        result: Result<VerifiedSource, SourceVerificationError>,
    }

    impl FakeVerifier {
        fn successful() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                result: Ok(VerifiedSource {
                    docker_context: "desktop-linux".to_owned(),
                    server_version: "8.4.4".parse().unwrap(),
                    vendor: "MySQL Community Server".to_owned(),
                    tls_cipher: Some("TLS_AES_256_GCM_SHA384".to_owned()),
                    client: ClientCatalog::resolve("8.4").unwrap(),
                }),
            }
        }
    }

    #[async_trait]
    impl SourceProfileVerifier for FakeVerifier {
        async fn verify(
            &self,
            _input: &NewProfileInput,
        ) -> Result<VerifiedSource, SourceVerificationError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result.clone()
        }
    }

    #[tokio::test]
    async fn verifies_and_persists_a_new_active_profile_and_credential() {
        let temp = TempDir::new().unwrap();
        let repository = repository(&temp);
        let store = crate::infrastructure::credentials::MemoryCredentialStore::default();
        let verifier = FakeVerifier::successful();

        let created = ProfileService::new(repository.clone())
            .add(&store, &verifier, new_profile_input("salt-source"))
            .await
            .unwrap();

        assert_eq!(created.name.as_str(), "salt-source");
        assert_eq!(created.server_version.to_string(), "8.4.4");
        assert_eq!(verifier.calls.load(Ordering::SeqCst), 1);
        let config = repository.load().unwrap();
        assert_eq!(config.active_profile.unwrap().as_str(), "salt-source");
        assert_eq!(
            config.client_runtime.docker_context.as_deref(),
            Some("desktop-linux")
        );
        let profile = config
            .profiles
            .get(&ProfileName::try_from("salt-source").unwrap())
            .unwrap();
        assert_eq!(profile.mysql_series, "8.4");
        assert_eq!(profile.tls_mode, MysqlTlsMode::Preferred);
        assert_eq!(
            store
                .get(&profile.credential_key)
                .await
                .unwrap()
                .expose_secret(),
            "password-that-must-not-leak"
        );
    }

    #[tokio::test]
    async fn unsupported_server_error_reports_the_safe_detected_series() {
        let temp = TempDir::new().unwrap();
        let repository = repository(&temp);
        let store = crate::infrastructure::credentials::MemoryCredentialStore::default();
        let verifier = FakeVerifier {
            calls: AtomicUsize::new(0),
            result: Err(SourceVerificationError::UnsupportedServerSeries {
                major: 5,
                minor: 7,
                supported: "8.0, 8.4",
            }),
        };

        let error = ProfileService::new(repository.clone())
            .add(&store, &verifier, new_profile_input("sandbox"))
            .await
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "the detected MySQL server series 5.7 is not supported by the approved client catalog (currently: 8.0, 8.4)"
        );
        assert!(matches!(
            error,
            ProfileServiceError::Verification(SourceVerificationError::UnsupportedServerSeries {
                major: 5,
                minor: 7,
                supported: "8.0, 8.4"
            })
        ));
        assert!(repository.load().unwrap().profiles.is_empty());
    }

    #[tokio::test]
    async fn duplicate_profile_is_rejected_before_connection_or_credentials() {
        let temp = TempDir::new().unwrap();
        let repository = repository(&temp);
        save_profiles(&repository);
        let store = crate::infrastructure::credentials::MemoryCredentialStore::default();
        let verifier = FakeVerifier::successful();

        let error = ProfileService::new(repository)
            .add(&store, &verifier, new_profile_input("local"))
            .await
            .unwrap_err();

        assert!(matches!(error, ProfileServiceError::AlreadyExists));
        assert_eq!(verifier.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn production_profile_requires_tls_before_connection() {
        let temp = TempDir::new().unwrap();
        let repository = repository(&temp);
        let verifier = FakeVerifier::successful();
        let mut input = new_profile_input("production");
        input.production = true;
        input.tls_mode = MysqlTlsMode::Preferred;

        let error = ProfileService::new(repository)
            .add(
                &crate::infrastructure::credentials::MemoryCredentialStore::default(),
                &verifier,
                input,
            )
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            ProfileServiceError::InvalidField {
                field: "tls_mode",
                ..
            }
        ));
        assert_eq!(verifier.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn connection_failure_leaves_config_and_credential_store_untouched() {
        let temp = TempDir::new().unwrap();
        let repository = repository(&temp);
        let verifier = FakeVerifier {
            calls: AtomicUsize::new(0),
            result: Err(SourceVerificationError::AuthenticationFailed),
        };

        let error = ProfileService::new(repository.clone())
            .add(
                &crate::infrastructure::credentials::MemoryCredentialStore::default(),
                &verifier,
                new_profile_input("salt-source"),
            )
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            ProfileServiceError::Verification(SourceVerificationError::AuthenticationFailed)
        ));
        assert_eq!(repository.load().unwrap(), AppConfig::default());
        assert!(!repository.paths().config_file().exists());
    }

    #[tokio::test]
    async fn required_tls_without_a_negotiated_cipher_is_not_persisted() {
        let temp = TempDir::new().unwrap();
        let repository = repository(&temp);
        let mut input = new_profile_input("tls-source");
        input.tls_mode = MysqlTlsMode::Required;
        let verifier = FakeVerifier {
            calls: AtomicUsize::new(0),
            result: Ok(VerifiedSource {
                tls_cipher: None,
                ..FakeVerifier::successful().result.unwrap()
            }),
        };

        let error = ProfileService::new(repository.clone())
            .add(
                &crate::infrastructure::credentials::MemoryCredentialStore::default(),
                &verifier,
                input,
            )
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            ProfileServiceError::Verification(SourceVerificationError::TlsRequiredButNotNegotiated)
        ));
        assert_eq!(repository.load().unwrap(), AppConfig::default());
        assert!(!repository.paths().config_file().exists());
    }

    #[test]
    fn lists_profiles_in_stable_order_with_only_safe_fields() {
        let temp = TempDir::new().unwrap();
        let repository = repository(&temp);
        save_profiles(&repository);
        let profiles = ProfileService::new(repository).list().unwrap();

        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0].name.as_str(), "local");
        assert!(profiles[0].active);
        assert_eq!(profiles[0].host, "127.0.0.1");
        assert_eq!(profiles[1].name.as_str(), "production");
        assert!(profiles[1].production);
    }

    #[test]
    fn activates_an_existing_profile_transactionally() {
        let temp = TempDir::new().unwrap();
        let repository = repository(&temp);
        save_profiles(&repository);
        let service = ProfileService::new(repository.clone());

        service
            .activate(&ProfileName::try_from("production").unwrap())
            .unwrap();

        assert_eq!(
            repository.load().unwrap().active_profile.unwrap().as_str(),
            "production"
        );
    }

    #[test]
    fn refusing_an_unknown_profile_does_not_change_configuration() {
        let temp = TempDir::new().unwrap();
        let repository = repository(&temp);
        save_profiles(&repository);
        let before = repository.load().unwrap();
        let service = ProfileService::new(repository.clone());

        let error = service
            .activate(&ProfileName::try_from("missing").unwrap())
            .unwrap_err();

        assert!(matches!(error, ProfileServiceError::NotFound));
        assert_eq!(repository.load().unwrap(), before);
    }

    #[tokio::test]
    async fn removes_config_reference_before_deleting_the_credential() {
        let temp = TempDir::new().unwrap();
        let repository = repository(&temp);
        save_profiles(&repository);
        let name = ProfileName::try_from("local").unwrap();
        let key = repository
            .load()
            .unwrap()
            .profiles
            .get(&name)
            .unwrap()
            .credential_key;
        let store = crate::infrastructure::credentials::MemoryCredentialStore::default();
        store
            .set(&key, secrecy::SecretString::from("secret"))
            .await
            .unwrap();
        let service = ProfileService::new(repository.clone());

        let removal = service.remove(&store, &name).await.unwrap();

        assert!(removal.was_active);
        assert!(!removal.credential_was_missing);
        let config = repository.load().unwrap();
        assert!(!config.profiles.contains_key(&name));
        assert_eq!(config.active_profile, None);
        assert_eq!(
            store.get(&key).await.unwrap_err(),
            CredentialError::NotFound
        );
    }

    #[tokio::test]
    async fn missing_credential_does_not_prevent_profile_removal() {
        let temp = TempDir::new().unwrap();
        let repository = repository(&temp);
        save_profiles(&repository);
        let name = ProfileName::try_from("production").unwrap();
        let service = ProfileService::new(repository.clone());

        let removal = service
            .remove(
                &crate::infrastructure::credentials::MemoryCredentialStore::default(),
                &name,
            )
            .await
            .unwrap();

        assert!(!removal.was_active);
        assert!(removal.credential_was_missing);
        assert!(!repository.load().unwrap().profiles.contains_key(&name));
    }

    struct OrderCheckingStore {
        repository: ConfigRepository,
        removed_name: ProfileName,
    }

    #[async_trait::async_trait]
    impl CredentialStore for OrderCheckingStore {
        async fn get(
            &self,
            _key: &CredentialKey,
        ) -> Result<secrecy::SecretString, CredentialError> {
            unreachable!()
        }

        async fn set(
            &self,
            _key: &CredentialKey,
            _value: secrecy::SecretString,
        ) -> Result<(), CredentialError> {
            unreachable!()
        }

        async fn delete(&self, _key: &CredentialKey) -> Result<(), CredentialError> {
            assert!(
                !self
                    .repository
                    .load()
                    .unwrap()
                    .profiles
                    .contains_key(&self.removed_name),
                "the config must stop referencing a credential before its deletion"
            );
            Ok(())
        }
    }

    #[tokio::test]
    async fn persists_profile_removal_before_touching_the_credential_store() {
        let temp = TempDir::new().unwrap();
        let repository = repository(&temp);
        save_profiles(&repository);
        let name = ProfileName::try_from("production").unwrap();
        let store = OrderCheckingStore {
            repository: repository.clone(),
            removed_name: name.clone(),
        };

        ProfileService::new(repository)
            .remove(&store, &name)
            .await
            .unwrap();
    }

    struct DeleteFailingStore;

    #[async_trait::async_trait]
    impl CredentialStore for DeleteFailingStore {
        async fn get(
            &self,
            _key: &CredentialKey,
        ) -> Result<secrecy::SecretString, CredentialError> {
            unreachable!()
        }

        async fn set(
            &self,
            _key: &CredentialKey,
            _value: secrecy::SecretString,
        ) -> Result<(), CredentialError> {
            unreachable!()
        }

        async fn delete(&self, _key: &CredentialKey) -> Result<(), CredentialError> {
            Err(CredentialError::StoreUnavailable {
                operation: crate::infrastructure::credentials::CredentialOperation::Delete,
            })
        }
    }

    #[tokio::test]
    async fn reports_an_orphan_key_when_credential_cleanup_fails() {
        let temp = TempDir::new().unwrap();
        let repository = repository(&temp);
        save_profiles(&repository);
        let name = ProfileName::try_from("production").unwrap();
        let expected_key = repository
            .load()
            .unwrap()
            .profiles
            .get(&name)
            .unwrap()
            .credential_key;
        let service = ProfileService::new(repository.clone());

        let error = service
            .remove(&DeleteFailingStore, &name)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            ProfileServiceError::CredentialCleanup { orphaned_key, .. }
                if orphaned_key == expected_key
        ));
        assert!(!repository.load().unwrap().profiles.contains_key(&name));
    }
}
