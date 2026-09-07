use std::{collections::HashMap, sync::RwLock};

#[cfg(feature = "test-file-credential-store")]
use std::{
    fs::{self, File},
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use thiserror::Error;

use crate::domain::CredentialKey;

#[cfg(feature = "test-file-credential-store")]
use sha2::{Digest as _, Sha256};

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

/// Credential backend selected by the CLI composition root.
///
/// Release builds always use the native OS store. A build explicitly compiled
/// with `test-file-credential-store` may opt into the isolated file backend by
/// setting `REPRODB_TEST_CREDENTIAL_DIR` to an absolute directory. This hook is
/// intentionally absent from normal binaries.
#[derive(Debug)]
pub enum RuntimeCredentialStore {
    Os(OsCredentialStore),

    #[cfg(feature = "test-file-credential-store")]
    TestFile(TestFileCredentialStore),
}

impl RuntimeCredentialStore {
    pub fn discover() -> Self {
        #[cfg(feature = "test-file-credential-store")]
        if let Some(root) = std::env::var_os("REPRODB_TEST_CREDENTIAL_DIR") {
            return Self::TestFile(TestFileCredentialStore::new(PathBuf::from(root)));
        }

        Self::Os(OsCredentialStore)
    }
}

#[async_trait]
impl CredentialStore for RuntimeCredentialStore {
    async fn get(&self, key: &CredentialKey) -> Result<SecretString, CredentialError> {
        match self {
            Self::Os(store) => store.get(key).await,
            #[cfg(feature = "test-file-credential-store")]
            Self::TestFile(store) => store.get(key).await,
        }
    }

    async fn set(&self, key: &CredentialKey, value: SecretString) -> Result<(), CredentialError> {
        match self {
            Self::Os(store) => store.set(key, value).await,
            #[cfg(feature = "test-file-credential-store")]
            Self::TestFile(store) => store.set(key, value).await,
        }
    }

    async fn delete(&self, key: &CredentialKey) -> Result<(), CredentialError> {
        match self {
            Self::Os(store) => store.delete(key).await,
            #[cfg(feature = "test-file-credential-store")]
            Self::TestFile(store) => store.delete(key).await,
        }
    }
}

#[cfg(feature = "test-file-credential-store")]
#[derive(Debug)]
pub struct TestFileCredentialStore {
    root: PathBuf,
}

#[cfg(feature = "test-file-credential-store")]
impl TestFileCredentialStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn path(root: &Path, key: &CredentialKey) -> PathBuf {
        let digest = Sha256::digest(key.to_string().as_bytes());
        root.join(format!("{digest:x}.credential"))
    }

    fn prepare_root(root: &Path, operation: CredentialOperation) -> Result<(), CredentialError> {
        if !root.is_absolute() {
            return Err(CredentialError::StoreUnavailable { operation });
        }
        fs::create_dir_all(root).map_err(|_| CredentialError::StoreUnavailable { operation })?;
        set_private_directory_permissions(root)
            .map_err(|_| CredentialError::StoreUnavailable { operation })
    }
}

#[cfg(feature = "test-file-credential-store")]
#[async_trait]
impl CredentialStore for TestFileCredentialStore {
    async fn get(&self, key: &CredentialKey) -> Result<SecretString, CredentialError> {
        let root = self.root.clone();
        let key = *key;
        tokio::task::spawn_blocking(move || {
            Self::prepare_root(&root, CredentialOperation::Get)?;
            let path = Self::path(&root, &key);
            let mut file = File::open(path).map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => CredentialError::NotFound,
                _ => CredentialError::OperationFailed {
                    operation: CredentialOperation::Get,
                },
            })?;
            let mut bytes = Vec::new();
            std::io::Read::take(&mut file, 4097)
                .read_to_end(&mut bytes)
                .map_err(|_| CredentialError::OperationFailed {
                    operation: CredentialOperation::Get,
                })?;
            if bytes.is_empty() || bytes.len() > 4096 {
                return Err(CredentialError::OperationFailed {
                    operation: CredentialOperation::Get,
                });
            }
            String::from_utf8(bytes)
                .map(SecretString::from)
                .map_err(|_| CredentialError::OperationFailed {
                    operation: CredentialOperation::Get,
                })
        })
        .await
        .map_err(|_| CredentialError::BackgroundTaskFailed {
            operation: CredentialOperation::Get,
        })?
    }

    async fn set(&self, key: &CredentialKey, value: SecretString) -> Result<(), CredentialError> {
        let root = self.root.clone();
        let key = *key;
        tokio::task::spawn_blocking(move || {
            Self::prepare_root(&root, CredentialOperation::Set)?;
            let path = Self::path(&root, &key);
            let mut temporary = tempfile::NamedTempFile::new_in(&root).map_err(|_| {
                CredentialError::OperationFailed {
                    operation: CredentialOperation::Set,
                }
            })?;
            set_private_file_permissions(temporary.as_file()).map_err(|_| {
                CredentialError::OperationFailed {
                    operation: CredentialOperation::Set,
                }
            })?;
            temporary
                .write_all(value.expose_secret().as_bytes())
                .and_then(|()| temporary.flush())
                .map_err(|_| CredentialError::OperationFailed {
                    operation: CredentialOperation::Set,
                })?;
            temporary
                .persist(path)
                .map_err(|_| CredentialError::OperationFailed {
                    operation: CredentialOperation::Set,
                })?;
            Ok(())
        })
        .await
        .map_err(|_| CredentialError::BackgroundTaskFailed {
            operation: CredentialOperation::Set,
        })?
    }

    async fn delete(&self, key: &CredentialKey) -> Result<(), CredentialError> {
        let root = self.root.clone();
        let key = *key;
        tokio::task::spawn_blocking(move || {
            Self::prepare_root(&root, CredentialOperation::Delete)?;
            fs::remove_file(Self::path(&root, &key)).map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => CredentialError::NotFound,
                _ => CredentialError::OperationFailed {
                    operation: CredentialOperation::Delete,
                },
            })
        })
        .await
        .map_err(|_| CredentialError::BackgroundTaskFailed {
            operation: CredentialOperation::Delete,
        })?
    }
}

#[cfg(all(feature = "test-file-credential-store", unix))]
fn set_private_directory_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(all(feature = "test-file-credential-store", not(unix)))]
fn set_private_directory_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(all(feature = "test-file-credential-store", unix))]
fn set_private_file_permissions(file: &File) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(all(feature = "test-file-credential-store", not(unix)))]
fn set_private_file_permissions(_file: &File) -> std::io::Result<()> {
    Ok(())
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

    #[cfg(feature = "test-file-credential-store")]
    #[tokio::test]
    async fn test_file_store_persists_without_using_a_credential_name_as_a_path() {
        let temporary = tempfile::TempDir::new().unwrap();
        let store = TestFileCredentialStore::new(temporary.path().join("credentials"));
        let key = CredentialKey::new(CredentialScope::Source);

        store
            .set(&key, SecretString::from("isolated-ci-password"))
            .await
            .unwrap();
        assert_eq!(
            store.get(&key).await.unwrap().expose_secret(),
            "isolated-ci-password"
        );
        store
            .set(&key, SecretString::from("replacement-ci-password"))
            .await
            .unwrap();
        assert_eq!(
            store.get(&key).await.unwrap().expose_secret(),
            "replacement-ci-password"
        );
        let entries = fs::read_dir(temporary.path().join("credentials"))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].file_name().to_string_lossy().contains("source"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            assert_eq!(
                fs::metadata(temporary.path().join("credentials"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                entries[0].metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        store.delete(&key).await.unwrap();
        assert_eq!(
            store.get(&key).await.unwrap_err(),
            CredentialError::NotFound
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
