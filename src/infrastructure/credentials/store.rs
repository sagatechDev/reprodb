use std::{collections::HashMap, sync::RwLock};

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use thiserror::Error;

use crate::domain::CredentialKey;

const DEFAULT_SERVICE: &str = "com.sagatech.reprodb";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialOperation {
    Get,
    Set,
    Delete,
}

impl std::fmt::Display for CredentialOperation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Get => "read",
            Self::Set => "save",
            Self::Delete => "delete",
        })
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum CredentialError {
    #[error("credential was not found in the operating system credential store")]
    NotFound,

    #[error(
        "the operating system credential store is unavailable while trying to {operation} a credential"
    )]
    StoreUnavailable { operation: CredentialOperation },

    #[error("the operating system credential store failed to {operation} a credential")]
    OperationFailed { operation: CredentialOperation },

    #[error("the background credential task failed while trying to {operation} a credential")]
    BackgroundTaskFailed { operation: CredentialOperation },
}

#[async_trait]
pub trait CredentialStore: Send + Sync {
    async fn get(&self, key: &CredentialKey) -> Result<SecretString, CredentialError>;
    async fn set(&self, key: &CredentialKey, value: SecretString) -> Result<(), CredentialError>;
    async fn delete(&self, key: &CredentialKey) -> Result<(), CredentialError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct OsCredentialStore;

impl OsCredentialStore {
    async fn run<T>(
        operation: CredentialOperation,
        task: impl FnOnce() -> keyring::Result<T> + Send + 'static,
    ) -> Result<T, CredentialError>
    where
        T: Send + 'static,
    {
        tokio::task::spawn_blocking(task)
            .await
            .map_err(|_| CredentialError::BackgroundTaskFailed { operation })?
            .map_err(|error| map_keyring_error(operation, error))
    }
}

#[async_trait]
impl CredentialStore for OsCredentialStore {
    async fn get(&self, key: &CredentialKey) -> Result<SecretString, CredentialError> {
        let account = key.to_string();
        Self::run(CredentialOperation::Get, move || {
            keyring::Entry::new(DEFAULT_SERVICE, &account)?.get_password()
        })
        .await
        .map(SecretString::from)
    }

    async fn set(&self, key: &CredentialKey, value: SecretString) -> Result<(), CredentialError> {
        let account = key.to_string();
        Self::run(CredentialOperation::Set, move || {
            keyring::Entry::new(DEFAULT_SERVICE, &account)?.set_password(value.expose_secret())
        })
        .await
    }

    async fn delete(&self, key: &CredentialKey) -> Result<(), CredentialError> {
        let account = key.to_string();
        Self::run(CredentialOperation::Delete, move || {
            keyring::Entry::new(DEFAULT_SERVICE, &account)?.delete_credential()
        })
        .await
    }
}

fn map_keyring_error(operation: CredentialOperation, error: keyring::Error) -> CredentialError {
    match error {
        keyring::Error::NoEntry => CredentialError::NotFound,
        keyring::Error::NoDefaultStore | keyring::Error::NoStorageAccess(_) => {
            CredentialError::StoreUnavailable { operation }
        }
        _ => CredentialError::OperationFailed { operation },
    }
}

#[derive(Default)]
pub struct MemoryCredentialStore {
    values: RwLock<HashMap<CredentialKey, SecretString>>,
}

#[async_trait]
impl CredentialStore for MemoryCredentialStore {
    async fn get(&self, key: &CredentialKey) -> Result<SecretString, CredentialError> {
        self.values
            .read()
            .map_err(|_| CredentialError::StoreUnavailable {
                operation: CredentialOperation::Get,
            })?
            .get(key)
            .cloned()
            .ok_or(CredentialError::NotFound)
    }

    async fn set(&self, key: &CredentialKey, value: SecretString) -> Result<(), CredentialError> {
        self.values
            .write()
            .map_err(|_| CredentialError::StoreUnavailable {
                operation: CredentialOperation::Set,
            })?
            .insert(*key, value);
        Ok(())
    }

    async fn delete(&self, key: &CredentialKey) -> Result<(), CredentialError> {
        let removed = self
            .values
            .write()
            .map_err(|_| CredentialError::StoreUnavailable {
                operation: CredentialOperation::Delete,
            })?
            .remove(key);
        removed.map(|_| ()).ok_or(CredentialError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::CredentialScope;

    #[tokio::test]
    async fn memory_store_obeys_the_full_contract() {
        let store = MemoryCredentialStore::default();
        let key = CredentialKey::new(CredentialScope::Source);
        let value = SecretString::from("password-that-must-not-leak");

        assert_eq!(
            store.get(&key).await.unwrap_err(),
            CredentialError::NotFound
        );
        store.set(&key, value.clone()).await.unwrap();
        assert_eq!(
            store.get(&key).await.unwrap().expose_secret(),
            value.expose_secret()
        );
        store.delete(&key).await.unwrap();
        assert_eq!(
            store.get(&key).await.unwrap_err(),
            CredentialError::NotFound
        );
    }

    #[tokio::test]
    async fn memory_store_replaces_an_existing_value() {
        let store = MemoryCredentialStore::default();
        let key = CredentialKey::new(CredentialScope::Target);

        store
            .set(&key, SecretString::from("first-password"))
            .await
            .unwrap();
        store
            .set(&key, SecretString::from("second-password"))
            .await
            .unwrap();

        assert_eq!(
            store.get(&key).await.unwrap().expose_secret(),
            "second-password"
        );
    }

    #[test]
    fn credential_errors_never_contain_a_secret_value() {
        let marker = "password-that-must-not-leak";
        let error = CredentialError::OperationFailed {
            operation: CredentialOperation::Set,
        };

        assert!(!error.to_string().contains(marker));
        assert!(!format!("{error:?}").contains(marker));
        assert!(!format!("{:?}", SecretString::from(marker)).contains(marker));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    #[ignore = "touches the native OS credential store"]
    async fn native_store_roundtrips_a_temporary_credential() {
        let store = OsCredentialStore;
        let key = CredentialKey::new(CredentialScope::Source);
        let value = SecretString::from("reprodb-native-store-integration-test");

        store.set(&key, value.clone()).await.unwrap();
        let loaded = store.get(&key).await;
        let cleanup = store.delete(&key).await;

        assert_eq!(loaded.unwrap().expose_secret(), value.expose_secret());
        cleanup.unwrap();
    }
}
