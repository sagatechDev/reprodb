use secrecy::SecretString;
use thiserror::Error;

use crate::{
    domain::CredentialKey,
    infrastructure::{
        config::{AppConfig, ConfigError, ConfigRepository},
        credentials::{CredentialError, CredentialStore},
    },
};

#[derive(Debug, Error)]
pub enum CredentialProvisionError {
    #[error(transparent)]
    Credential(#[from] CredentialError),

    #[error("the configuration must reference the new credential exactly once before it is saved")]
    InvalidCredentialReference,

    #[error("refusing to replace an existing credential during profile provisioning")]
    CredentialAlreadyExists,

    #[error("could not persist the configuration after saving its credential")]
    Config {
        #[source]
        source: ConfigError,
    },

    #[error(
        "could not persist the configuration or remove its orphaned credential `{orphaned_key}`"
    )]
    ConfigAndRollback {
        orphaned_key: CredentialKey,
        config_error: ConfigError,
        rollback_error: CredentialError,
    },
}

pub async fn persist_config_with_credential(
    store: &dyn CredentialStore,
    repository: &ConfigRepository,
    key: CredentialKey,
    value: SecretString,
    config: &AppConfig,
) -> Result<(), CredentialProvisionError> {
    config
        .validate()
        .map_err(|source| CredentialProvisionError::Config { source })?;

    let reference_count = config
        .local_target
        .iter()
        .filter(|target| target.credential_key == key)
        .count()
        + config
            .local_targets
            .values()
            .filter(|target| target.credential_key == key)
            .count()
        + config
            .profiles
            .values()
            .filter(|profile| profile.credential_key == key)
            .count();
    if reference_count != 1 {
        return Err(CredentialProvisionError::InvalidCredentialReference);
    }

    match store.get(&key).await {
        Ok(_) => return Err(CredentialProvisionError::CredentialAlreadyExists),
        Err(CredentialError::NotFound) => {}
        Err(error) => return Err(error.into()),
    }

    store.set(&key, value).await?;

    if let Err(config_error) = repository.save(config) {
        return match store.delete(&key).await {
            Ok(()) => Err(CredentialProvisionError::Config {
                source: config_error,
            }),
            Err(rollback_error) => Err(CredentialProvisionError::ConfigAndRollback {
                orphaned_key: key,
                config_error,
                rollback_error,
            }),
        };
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use secrecy::ExposeSecret;
    use tempfile::TempDir;

    use super::*;
    use crate::{
        domain::{ContainerId, ContainerName, CredentialScope, DatabaseName},
        infrastructure::{
            config::{AppPaths, LocalTargetConfig},
            credentials::{CredentialError, MemoryCredentialStore},
        },
    };

    fn repository(temp: &TempDir) -> ConfigRepository {
        ConfigRepository::new(AppPaths::new(
            temp.path().join("config"),
            temp.path().join("cache"),
            temp.path().join("data"),
        ))
    }

    fn failing_repository(temp: &TempDir) -> ConfigRepository {
        let config_dir = temp.path().join("not-a-directory");
        std::fs::write(&config_dir, "occupied").unwrap();
        ConfigRepository::new(AppPaths::new(
            config_dir,
            temp.path().join("cache"),
            temp.path().join("data"),
        ))
    }

    fn config_with_credential(key: CredentialKey) -> AppConfig {
        AppConfig {
            local_target: Some(LocalTargetConfig {
                docker_context: "desktop-linux".to_owned(),
                container_name: ContainerName::try_from("mysql-8").unwrap(),
                container_id: ContainerId::try_from("a".repeat(64)).unwrap(),
                username: "root".to_owned(),
                credential_key: key,
                central_database: DatabaseName::try_from("salt_central").unwrap(),
                trust: crate::infrastructure::config::LocalTargetTrust::UserConfirmed,
                legacy_tenant_database_prefix: None,
            }),
            ..AppConfig::default()
        }
    }

    struct DeleteFailingStore;

    #[async_trait]
    impl CredentialStore for DeleteFailingStore {
        async fn get(&self, _key: &CredentialKey) -> Result<SecretString, CredentialError> {
            Err(CredentialError::NotFound)
        }

        async fn set(
            &self,
            _key: &CredentialKey,
            _value: SecretString,
        ) -> Result<(), CredentialError> {
            Ok(())
        }

        async fn delete(&self, _key: &CredentialKey) -> Result<(), CredentialError> {
            Err(CredentialError::OperationFailed {
                operation: crate::infrastructure::credentials::CredentialOperation::Delete,
            })
        }
    }

    #[tokio::test]
    async fn persists_the_credential_before_the_valid_configuration() {
        let temp = TempDir::new().unwrap();
        let repository = repository(&temp);
        let store = MemoryCredentialStore::default();
        let key = CredentialKey::new(CredentialScope::Target);
        let config = config_with_credential(key);

        persist_config_with_credential(
            &store,
            &repository,
            key,
            SecretString::from("secret"),
            &config,
        )
        .await
        .unwrap();

        assert_eq!(store.get(&key).await.unwrap().expose_secret(), "secret");
        assert!(repository.paths().config_file().exists());
    }

    #[tokio::test]
    async fn rolls_back_the_credential_when_config_persistence_fails() {
        let temp = TempDir::new().unwrap();
        let repository = failing_repository(&temp);
        let store = MemoryCredentialStore::default();
        let key = CredentialKey::new(CredentialScope::Target);
        let config = config_with_credential(key);

        let error = persist_config_with_credential(
            &store,
            &repository,
            key,
            SecretString::from("password-that-must-not-leak"),
            &config,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, CredentialProvisionError::Config { .. }));
        assert_eq!(
            store.get(&key).await.unwrap_err(),
            CredentialError::NotFound
        );
        assert!(!error.to_string().contains("password-that-must-not-leak"));
        assert!(!format!("{error:?}").contains("password-that-must-not-leak"));
    }

    #[tokio::test]
    async fn refuses_to_overwrite_an_existing_credential() {
        let temp = TempDir::new().unwrap();
        let repository = repository(&temp);
        let store = MemoryCredentialStore::default();
        let key = CredentialKey::new(CredentialScope::Target);
        store
            .set(&key, SecretString::from("existing-secret"))
            .await
            .unwrap();

        let error = persist_config_with_credential(
            &store,
            &repository,
            key,
            SecretString::from("replacement-secret"),
            &config_with_credential(key),
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            CredentialProvisionError::CredentialAlreadyExists
        ));
        assert_eq!(
            store.get(&key).await.unwrap().expose_secret(),
            "existing-secret"
        );
        assert!(!repository.paths().config_file().exists());
    }

    #[tokio::test]
    async fn refuses_an_unreferenced_credential_before_touching_the_store() {
        let temp = TempDir::new().unwrap();
        let repository = repository(&temp);
        let store = MemoryCredentialStore::default();
        let key = CredentialKey::new(CredentialScope::Target);

        let error = persist_config_with_credential(
            &store,
            &repository,
            key,
            SecretString::from("secret"),
            &AppConfig::default(),
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            CredentialProvisionError::InvalidCredentialReference
        ));
        assert_eq!(
            store.get(&key).await.unwrap_err(),
            CredentialError::NotFound
        );
    }

    #[tokio::test]
    async fn reports_the_orphaned_key_when_config_and_rollback_both_fail() {
        let temp = TempDir::new().unwrap();
        let repository = failing_repository(&temp);
        let key = CredentialKey::new(CredentialScope::Target);
        let config = config_with_credential(key);

        let error = persist_config_with_credential(
            &DeleteFailingStore,
            &repository,
            key,
            SecretString::from("password-that-must-not-leak"),
            &config,
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            CredentialProvisionError::ConfigAndRollback {
                orphaned_key,
                ..
            } if orphaned_key == key
        ));
        assert!(error.to_string().contains(&key.to_string()));
        assert!(!error.to_string().contains("password-that-must-not-leak"));
        assert!(!format!("{error:?}").contains("password-that-must-not-leak"));
    }

    #[tokio::test]
    async fn rejects_an_invalid_config_before_touching_the_store() {
        let temp = TempDir::new().unwrap();
        let repository = repository(&temp);
        let store = MemoryCredentialStore::default();
        let key = CredentialKey::new(CredentialScope::Target);
        let config = AppConfig {
            schema_version: u32::MAX,
            ..config_with_credential(key)
        };

        let error = persist_config_with_credential(
            &store,
            &repository,
            key,
            SecretString::from("secret"),
            &config,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, CredentialProvisionError::Config { .. }));
        assert_eq!(
            store.get(&key).await.unwrap_err(),
            CredentialError::NotFound
        );
    }
}
