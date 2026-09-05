use thiserror::Error;

use crate::{
    domain::{CredentialKey, ProfileName},
    infrastructure::{
        config::{ConfigError, ConfigRepository},
        credentials::{CredentialError, CredentialStore},
    },
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileSummary {
    pub name: ProfileName,
    pub active: bool,
    pub host: String,
    pub port: u16,
    pub mysql_series: String,
    pub production: bool,
}

#[derive(Debug, Error)]
pub enum ProfileServiceError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error("source profile does not exist; run `reprodb profile list`")]
    NotFound,

    #[error(
        "profile was removed, but credential `{orphaned_key}` could not be deleted; retry cleanup from the OS credential store"
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
                production: profile.production,
            })
            .collect())
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tempfile::TempDir;

    use super::*;
    use crate::{
        domain::{CredentialScope, DatabaseName},
        infrastructure::{
            config::{
                AppConfig, AppPaths, MysqlClientConfig, MysqlFamily, SourceProfileConfig,
                TenantResolverConfig,
            },
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
            client: MysqlClientConfig {
                image: client.image().to_owned(),
            },
            tenant_resolver: TenantResolverConfig::SaltCentral {
                central_database: DatabaseName::try_from("salt_central").unwrap(),
                allow_domain_lookup: true,
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
