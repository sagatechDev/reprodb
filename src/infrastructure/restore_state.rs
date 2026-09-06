use std::{
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::{
    application::AuthorizedLocalTarget,
    domain::{DumpId, Sha256Digest},
};

const RESTORES_DIRECTORY: &str = "restores";
const RESTORE_STATE_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestoreStatus {
    Incomplete,
    Ready,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreState {
    pub format_version: u32,
    pub docker_context: String,
    pub container_id: String,
    pub container_name: String,
    pub database: String,
    pub dump_id: DumpId,
    pub status: RestoreStatus,
    pub updated_at_unix_seconds: u64,
}

#[derive(Clone, Debug)]
pub struct LocalRestoreStateStore {
    data_root: PathBuf,
}

impl LocalRestoreStateStore {
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        Self {
            data_root: data_root.into(),
        }
    }

    pub fn save(
        &self,
        target: &AuthorizedLocalTarget,
        dump_id: DumpId,
        status: RestoreStatus,
    ) -> Result<RestoreState, RestoreStateError> {
        let directory = self.data_root.join(RESTORES_DIRECTORY);
        fs::create_dir_all(&directory).map_err(RestoreStateError::CreateDirectory)?;
        set_private_directory_permissions(&self.data_root)
            .and_then(|_| set_private_directory_permissions(&directory))
            .map_err(RestoreStateError::RestrictDirectory)?;

        let state = RestoreState {
            format_version: RESTORE_STATE_VERSION,
            docker_context: target.docker_context().to_owned(),
            container_id: target.container_id().to_string(),
            container_name: target.container_name().to_string(),
            database: target.database().to_string(),
            dump_id,
            status,
            updated_at_unix_seconds: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| RestoreStateError::Clock)?
                .as_secs(),
        };
        let path = directory.join(format!("{}.json", target_digest(target)));
        let mut temporary =
            NamedTempFile::new_in(&directory).map_err(RestoreStateError::CreateTemporaryFile)?;
        set_private_file_permissions(temporary.as_file())
            .map_err(RestoreStateError::RestrictFile)?;
        serde_json::to_writer_pretty(&mut temporary, &state)
            .map_err(RestoreStateError::Serialize)?;
        use std::io::Write as _;
        temporary
            .write_all(b"\n")
            .and_then(|_| temporary.flush())
            .and_then(|_| temporary.as_file().sync_all())
            .map_err(RestoreStateError::Write)?;
        temporary
            .persist(&path)
            .map_err(|error| RestoreStateError::Persist(error.error))?;
        sync_directory(&directory)?;
        Ok(state)
    }

    #[cfg(test)]
    pub fn load(&self, target: &AuthorizedLocalTarget) -> Result<RestoreState, RestoreStateError> {
        let path = self
            .data_root
            .join(RESTORES_DIRECTORY)
            .join(format!("{}.json", target_digest(target)));
        let file = File::open(path).map_err(RestoreStateError::Read)?;
        serde_json::from_reader(file).map_err(RestoreStateError::Deserialize)
    }
}

fn target_digest(target: &AuthorizedLocalTarget) -> Sha256Digest {
    let mut hasher = Sha256::new();
    hasher.update(b"reprodb-restore-state-v1");
    for value in [
        target.docker_context().as_bytes(),
        target.container_id().as_str().as_bytes(),
        target.database().as_str().as_bytes(),
    ] {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value);
    }
    Sha256Digest::from_bytes(hasher.finalize().into())
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

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), RestoreStateError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(RestoreStateError::SyncDirectory)
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), RestoreStateError> {
    Ok(())
}

#[derive(Debug, Error)]
pub enum RestoreStateError {
    #[error("could not create the local restore state directory")]
    CreateDirectory(#[source] io::Error),
    #[error("could not restrict the local restore state directory")]
    RestrictDirectory(#[source] io::Error),
    #[error("could not create a temporary restore state file")]
    CreateTemporaryFile(#[source] io::Error),
    #[error("could not restrict the local restore state file")]
    RestrictFile(#[source] io::Error),
    #[error("could not serialize local restore state")]
    Serialize(#[source] serde_json::Error),
    #[error("could not write local restore state")]
    Write(#[source] io::Error),
    #[error("could not publish local restore state")]
    Persist(#[source] io::Error),
    #[error("could not sync the local restore state directory")]
    SyncDirectory(#[source] io::Error),
    #[error("the system clock is before the Unix epoch")]
    Clock,
    #[cfg(test)]
    #[error("could not read local restore state")]
    Read(#[source] io::Error),
    #[cfg(test)]
    #[error("could not deserialize local restore state")]
    Deserialize(#[source] serde_json::Error),
}
