use std::{
    collections::HashMap,
    fs::{self, File},
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    sync::RwLock,
};

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use thiserror::Error;

use crate::domain::CredentialKey;

use sha2::{Digest as _, Sha256};

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
    #[error("credential was not found in the local credential store")]
    NotFound,

    #[error("the local credential store is unavailable while trying to {operation} a credential")]
    StoreUnavailable { operation: CredentialOperation },

    #[error("the local credential store failed to {operation} a credential")]
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

#[derive(Debug)]
pub struct RuntimeCredentialStore {
    local: FileCredentialStore,
}

impl RuntimeCredentialStore {
    pub fn discover(root: PathBuf) -> Self {
        Self {
            local: FileCredentialStore::new(root),
        }
    }
}

#[async_trait]
impl CredentialStore for RuntimeCredentialStore {
    async fn get(&self, key: &CredentialKey) -> Result<SecretString, CredentialError> {
        self.local.get(key).await
    }

    async fn set(&self, key: &CredentialKey, value: SecretString) -> Result<(), CredentialError> {
        self.local.set(key, value).await
    }

    async fn delete(&self, key: &CredentialKey) -> Result<(), CredentialError> {
        self.local.delete(key).await
    }
}

#[derive(Debug)]
pub struct FileCredentialStore {
    root: PathBuf,
}

impl FileCredentialStore {
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

#[async_trait]
impl CredentialStore for FileCredentialStore {
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

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(file: &File) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_file: &File) -> std::io::Result<()> {
    Ok(())
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

    #[tokio::test]
    async fn file_store_persists_with_private_permissions_and_hashed_names() {
        let temporary = tempfile::TempDir::new().unwrap();
        let store = FileCredentialStore::new(temporary.path().join("credentials"));
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
}
