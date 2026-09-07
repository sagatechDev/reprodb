use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
};

use fs4::TryLockError;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::domain::{ContainerId, DatabaseName, ProfileName, Sha256Digest};

const LOCKS_DIRECTORY: &str = "locks";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationLockScope {
    Source,
    Target,
}

impl OperationLockScope {
    const fn directory(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Target => "target",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperationLockKey {
    scope: OperationLockScope,
    digest: Sha256Digest,
}

impl OperationLockKey {
    pub fn source(profile: &ProfileName, database: &DatabaseName) -> Self {
        Self::new(
            OperationLockScope::Source,
            [profile.as_str().as_bytes(), database.as_str().as_bytes()],
        )
    }

    pub fn target(
        docker_context: &str,
        container_id: &ContainerId,
        database: &DatabaseName,
    ) -> Self {
        Self::new(
            OperationLockScope::Target,
            [
                docker_context.as_bytes(),
                container_id.as_str().as_bytes(),
                database.as_str().as_bytes(),
            ],
        )
    }

    fn new<'a>(scope: OperationLockScope, fields: impl IntoIterator<Item = &'a [u8]>) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"reprodb-operation-lock-v1");
        hash_field(&mut hasher, scope.directory().as_bytes());
        for field in fields {
            hash_field(&mut hasher, field);
        }
        Self {
            scope,
            digest: Sha256Digest::from_bytes(hasher.finalize().into()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct OperationLockManager {
    cache_root: PathBuf,
}

impl OperationLockManager {
    pub fn new(cache_root: impl Into<PathBuf>) -> Self {
        Self {
            cache_root: cache_root.into(),
        }
    }

    pub fn try_acquire(
        &self,
        key: OperationLockKey,
    ) -> Result<OperationLockGuard, OperationLockError> {
        let directory = self
            .cache_root
            .join(LOCKS_DIRECTORY)
            .join(key.scope.directory());
        create_private_directories(&self.cache_root, &directory)?;
        let path = directory.join(format!("{}.lock", key.digest));
        let file = open_private_lock_file(&path).map_err(|source| OperationLockError::Io {
            operation: "open an operation lock",
            source,
        })?;
        match fs4::FileExt::try_lock(&file) {
            Ok(()) => Ok(OperationLockGuard { _file: file }),
            Err(TryLockError::WouldBlock) => Err(OperationLockError::Busy { scope: key.scope }),
            Err(TryLockError::Error(source)) => Err(OperationLockError::Io {
                operation: "acquire an operation lock",
                source,
            }),
        }
    }
}

#[derive(Debug)]
pub struct OperationLockGuard {
    _file: File,
}

impl Drop for OperationLockGuard {
    fn drop(&mut self) {
        // Explicit unlock avoids relying on platform-specific close timing.
        // Closing the descriptor remains the final fallback if unlock fails.
        let _ = fs4::FileExt::unlock(&self._file);
    }
}

#[derive(Debug, Error)]
pub enum OperationLockError {
    #[error("another local {scope:?} operation is already running for this database")]
    Busy { scope: OperationLockScope },

    #[error("could not {operation}")]
    Io {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn create_private_directories(root: &Path, leaf: &Path) -> Result<(), OperationLockError> {
    fs::create_dir_all(leaf).map_err(|source| OperationLockError::Io {
        operation: "create operation lock directories",
        source,
    })?;
    for directory in [root.to_owned(), root.join(LOCKS_DIRECTORY), leaf.to_owned()] {
        set_private_directory_permissions(&directory).map_err(|source| OperationLockError::Io {
            operation: "restrict an operation lock directory",
            source,
        })?;
    }
    Ok(())
}

fn open_private_lock_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    set_private_file_permissions(&file)?;
    Ok(file)
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn profile() -> ProfileName {
        ProfileName::try_from("local-source").unwrap()
    }

    fn database(value: &str) -> DatabaseName {
        DatabaseName::try_from(value).unwrap()
    }

    fn container_id() -> ContainerId {
        ContainerId::try_from("a".repeat(64)).unwrap()
    }

    #[test]
    fn same_resource_is_exclusive_and_drop_releases_the_lock() {
        let directory = tempdir().unwrap();
        let manager = OperationLockManager::new(directory.path());
        let key = OperationLockKey::source(&profile(), &database("salt_sagatec"));

        let guard = manager.try_acquire(key).unwrap();
        assert!(matches!(
            manager.try_acquire(key),
            Err(OperationLockError::Busy {
                scope: OperationLockScope::Source
            })
        ));

        drop(guard);
        assert!(manager.try_acquire(key).is_ok());
    }

    #[test]
    fn source_target_and_different_databases_have_independent_locks() {
        let directory = tempdir().unwrap();
        let manager = OperationLockManager::new(directory.path());
        let source = OperationLockKey::source(&profile(), &database("salt_sagatec"));
        let other_source = OperationLockKey::source(&profile(), &database("salt_polymer"));
        let target =
            OperationLockKey::target("desktop-linux", &container_id(), &database("salt_sagatec"));

        let _source = manager.try_acquire(source).unwrap();
        let _other_source = manager.try_acquire(other_source).unwrap();
        let _target = manager.try_acquire(target).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn lock_files_are_private_and_do_not_contain_resource_names() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().unwrap();
        let manager = OperationLockManager::new(directory.path());
        let _guard = manager
            .try_acquire(OperationLockKey::source(
                &profile(),
                &database("salt_sagatec"),
            ))
            .unwrap();
        let lock_directory = directory.path().join("locks/source");
        let entry = fs::read_dir(&lock_directory)
            .unwrap()
            .next()
            .unwrap()
            .unwrap();

        assert_eq!(
            fs::metadata(lock_directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            entry.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!entry.file_name().to_string_lossy().contains("sagatec"));
    }
}
