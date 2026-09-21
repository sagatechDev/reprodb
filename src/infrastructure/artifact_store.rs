use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
};

use fs4::TryLockError;
use thiserror::Error;

use crate::domain::{DatabaseName, DumpArtifactMetadata, DumpId, ProfileName};
use crate::infrastructure::compression::CompressionMetrics;

pub(crate) const PROFILES_DIRECTORY: &str = "profiles";
pub(crate) const PART_SUFFIX: &str = ".part";
pub(crate) const DUMP_FILE_NAME: &str = "dump.sql.zst";
pub(crate) const METADATA_FILE_NAME: &str = "metadata.json";
pub(crate) const ARTIFACT_LOCK_FILE_NAME: &str = ".artifact.lock";
const METADATA_PART_FILE_NAME: &str = "metadata.json.part";

#[derive(Clone, Debug)]
pub struct LocalArtifactStore {
    root: PathBuf,
}

impl LocalArtifactStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn begin(
        &self,
        profile: &ProfileName,
        database: &DatabaseName,
    ) -> Result<StagedDumpArtifact, ArtifactStoreError> {
        self.begin_with_id(profile, database, DumpId::new())
    }

    fn begin_with_id(
        &self,
        profile: &ProfileName,
        database: &DatabaseName,
        dump_id: DumpId,
    ) -> Result<StagedDumpArtifact, ArtifactStoreError> {
        let parent = self
            .root
            .join(PROFILES_DIRECTORY)
            .join(profile.as_str())
            .join(database.as_str());
        create_private_directories(&self.root, &parent)?;

        let stage_path = parent.join(format!("{dump_id}{PART_SUFFIX}"));
        let published_path = parent.join(dump_id.to_string());
        fs::create_dir(&stage_path).map_err(|source| ArtifactStoreError::Io {
            operation: "create the staged artifact directory",
            source,
        })?;
        set_private_directory_permissions(&stage_path).map_err(|source| {
            ArtifactStoreError::Io {
                operation: "restrict the staged artifact directory",
                source,
            }
        })?;
        let activity_lock = match try_acquire_exclusive_artifact_lock(&stage_path) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                let _ = fs::remove_dir_all(&stage_path);
                return Err(ArtifactStoreError::ArtifactBusy);
            }
            Err(error) => {
                let _ = fs::remove_dir_all(&stage_path);
                return Err(error);
            }
        };

        Ok(StagedDumpArtifact {
            dump_id,
            profile: profile.clone(),
            database: database.clone(),
            parent,
            stage_path,
            published_path,
            activity_lock: Some(activity_lock),
            published: false,
        })
    }

    pub fn list_complete(
        &self,
        profile: &ProfileName,
        database: &DatabaseName,
    ) -> Result<Vec<PublishedDumpArtifact>, ArtifactStoreError> {
        let parent = self
            .root
            .join(PROFILES_DIRECTORY)
            .join(profile.as_str())
            .join(database.as_str());
        let entries = match fs::read_dir(&parent) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(ArtifactStoreError::Io {
                    operation: "list completed artifact directories",
                    source,
                });
            }
        };
        let mut artifacts = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| ArtifactStoreError::Io {
                operation: "read an artifact directory entry",
                source,
            })?;
            if !entry
                .file_type()
                .map_err(|source| ArtifactStoreError::Io {
                    operation: "inspect an artifact directory entry",
                    source,
                })?
                .is_dir()
            {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(dump_id) = name.parse::<DumpId>() else {
                continue;
            };
            let path = entry.path();
            let dump_path = path.join(DUMP_FILE_NAME);
            let metadata_path = path.join(METADATA_FILE_NAME);
            if is_regular_file(&dump_path)? && is_regular_file(&metadata_path)? {
                artifacts.push(PublishedDumpArtifact {
                    dump_id,
                    path,
                    dump_path,
                    metadata_path,
                });
            }
        }
        artifacts.sort_by_key(|artifact| artifact.dump_id.to_string());
        Ok(artifacts)
    }

    pub fn find_complete_by_id(
        &self,
        dump_id: DumpId,
    ) -> Result<Vec<LocatedDumpArtifact>, ArtifactStoreError> {
        let profiles_path = self.root.join(PROFILES_DIRECTORY);
        let profiles = match fs::read_dir(&profiles_path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(ArtifactStoreError::Io {
                    operation: "list artifact profiles",
                    source,
                });
            }
        };
        let mut artifacts = Vec::new();
        for profile_entry in profiles {
            let profile_entry = profile_entry.map_err(|source| ArtifactStoreError::Io {
                operation: "read an artifact profile entry",
                source,
            })?;
            if !profile_entry
                .file_type()
                .map_err(|source| ArtifactStoreError::Io {
                    operation: "inspect an artifact profile entry",
                    source,
                })?
                .is_dir()
            {
                continue;
            }
            let Some(profile_name) = profile_entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(profile) = ProfileName::try_from(profile_name) else {
                continue;
            };
            let databases =
                fs::read_dir(profile_entry.path()).map_err(|source| ArtifactStoreError::Io {
                    operation: "list artifact databases",
                    source,
                })?;
            for database_entry in databases {
                let database_entry = database_entry.map_err(|source| ArtifactStoreError::Io {
                    operation: "read an artifact database entry",
                    source,
                })?;
                if !database_entry
                    .file_type()
                    .map_err(|source| ArtifactStoreError::Io {
                        operation: "inspect an artifact database entry",
                        source,
                    })?
                    .is_dir()
                {
                    continue;
                }
                let Some(database_name) = database_entry.file_name().to_str().map(str::to_owned)
                else {
                    continue;
                };
                let Ok(database) = DatabaseName::try_from(database_name) else {
                    continue;
                };
                let path = database_entry.path().join(dump_id.to_string());
                if !is_regular_directory(&path)? {
                    continue;
                }
                let dump_path = path.join(DUMP_FILE_NAME);
                let metadata_path = path.join(METADATA_FILE_NAME);
                if !is_regular_file(&dump_path)? || !is_regular_file(&metadata_path)? {
                    continue;
                }
                artifacts.push(LocatedDumpArtifact {
                    profile: profile.clone(),
                    database,
                    artifact: PublishedDumpArtifact {
                        dump_id,
                        path,
                        dump_path,
                        metadata_path,
                    },
                });
            }
        }
        Ok(artifacts)
    }

    pub fn list_complete_for_profile(
        &self,
        profile: &ProfileName,
    ) -> Result<Vec<LocatedDumpArtifact>, ArtifactStoreError> {
        let profile_path = self.root.join(PROFILES_DIRECTORY).join(profile.as_str());
        let databases = match fs::read_dir(&profile_path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(ArtifactStoreError::Io {
                    operation: "list artifact databases for a profile",
                    source,
                });
            }
        };
        let mut artifacts = Vec::new();
        for database_entry in databases {
            let database_entry = database_entry.map_err(|source| ArtifactStoreError::Io {
                operation: "read an artifact database entry",
                source,
            })?;
            if !database_entry
                .file_type()
                .map_err(|source| ArtifactStoreError::Io {
                    operation: "inspect an artifact database entry",
                    source,
                })?
                .is_dir()
            {
                continue;
            }
            let Some(database_name) = database_entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(database) = DatabaseName::try_from(database_name) else {
                continue;
            };
            artifacts.extend(
                self.list_complete(profile, &database)?
                    .into_iter()
                    .map(|artifact| LocatedDumpArtifact {
                        profile: profile.clone(),
                        database: database.clone(),
                        artifact,
                    }),
            );
        }
        Ok(artifacts)
    }

    pub fn list_all_candidates(&self) -> Result<Vec<LocatedDumpArtifact>, ArtifactStoreError> {
        let profiles_path = self.root.join(PROFILES_DIRECTORY);
        let profiles = match fs::read_dir(&profiles_path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(ArtifactStoreError::Io {
                    operation: "list artifact profiles",
                    source,
                });
            }
        };
        let mut artifacts = Vec::new();
        for profile_entry in profiles {
            let profile_entry = profile_entry.map_err(|source| ArtifactStoreError::Io {
                operation: "read an artifact profile entry",
                source,
            })?;
            if !profile_entry
                .file_type()
                .map_err(|source| ArtifactStoreError::Io {
                    operation: "inspect an artifact profile entry",
                    source,
                })?
                .is_dir()
            {
                continue;
            }
            let Some(profile_name) = profile_entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(profile) = ProfileName::try_from(profile_name) else {
                continue;
            };
            let databases =
                fs::read_dir(profile_entry.path()).map_err(|source| ArtifactStoreError::Io {
                    operation: "list artifact databases",
                    source,
                })?;
            for database_entry in databases {
                let database_entry = database_entry.map_err(|source| ArtifactStoreError::Io {
                    operation: "read an artifact database entry",
                    source,
                })?;
                if !database_entry
                    .file_type()
                    .map_err(|source| ArtifactStoreError::Io {
                        operation: "inspect an artifact database entry",
                        source,
                    })?
                    .is_dir()
                {
                    continue;
                }
                let Some(database_name) = database_entry.file_name().to_str().map(str::to_owned)
                else {
                    continue;
                };
                let Ok(database) = DatabaseName::try_from(database_name) else {
                    continue;
                };
                let candidates = fs::read_dir(database_entry.path()).map_err(|source| {
                    ArtifactStoreError::Io {
                        operation: "list artifact candidates",
                        source,
                    }
                })?;
                for candidate in candidates {
                    let candidate = candidate.map_err(|source| ArtifactStoreError::Io {
                        operation: "read an artifact candidate entry",
                        source,
                    })?;
                    if !candidate
                        .file_type()
                        .map_err(|source| ArtifactStoreError::Io {
                            operation: "inspect an artifact candidate entry",
                            source,
                        })?
                        .is_dir()
                    {
                        continue;
                    }
                    let Some(candidate_name) = candidate.file_name().to_str().map(str::to_owned)
                    else {
                        continue;
                    };
                    let Ok(dump_id) = candidate_name.parse::<DumpId>() else {
                        continue;
                    };
                    let path = candidate.path();
                    artifacts.push(LocatedDumpArtifact {
                        profile: profile.clone(),
                        database: database.clone(),
                        artifact: PublishedDumpArtifact {
                            dump_id,
                            dump_path: path.join(DUMP_FILE_NAME),
                            metadata_path: path.join(METADATA_FILE_NAME),
                            path,
                        },
                    });
                }
            }
        }
        artifacts.sort_by_key(|located| {
            (
                located.profile.as_str().to_owned(),
                located.database.as_str().to_owned(),
                located.artifact.dump_id.to_string(),
            )
        });
        Ok(artifacts)
    }

    pub fn try_acquire_lease(
        &self,
        artifact: &PublishedDumpArtifact,
    ) -> Result<ArtifactLease, ArtifactLeaseError> {
        let file = match open_artifact_lock_file(&artifact.path.join(ARTIFACT_LOCK_FILE_NAME)) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(ArtifactLeaseError::NotFound);
            }
            Err(source) => {
                return Err(ArtifactLeaseError::Io {
                    operation: "open an artifact lease",
                    source,
                });
            }
        };
        match fs4::FileExt::try_lock_shared(&file) {
            Ok(()) => Ok(ArtifactLease { _file: file }),
            Err(TryLockError::WouldBlock) => Err(ArtifactLeaseError::Busy),
            Err(TryLockError::Error(source)) => Err(ArtifactLeaseError::Io {
                operation: "acquire an artifact lease",
                source,
            }),
        }
    }
}

fn is_regular_directory(path: &Path) -> Result<bool, ArtifactStoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_dir()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(ArtifactStoreError::Io {
            operation: "inspect a managed artifact directory",
            source,
        }),
    }
}

fn is_regular_file(path: &Path) -> Result<bool, ArtifactStoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_file()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(ArtifactStoreError::Io {
            operation: "inspect a managed artifact file",
            source,
        }),
    }
}

#[derive(Debug)]
pub struct StagedDumpArtifact {
    dump_id: DumpId,
    profile: ProfileName,
    database: DatabaseName,
    parent: PathBuf,
    stage_path: PathBuf,
    published_path: PathBuf,
    activity_lock: Option<File>,
    published: bool,
}

impl StagedDumpArtifact {
    pub const fn dump_id(&self) -> DumpId {
        self.dump_id
    }

    pub fn create_dump_writer(&self) -> Result<File, ArtifactStoreError> {
        create_private_file(&self.stage_path.join(DUMP_FILE_NAME)).map_err(|source| {
            ArtifactStoreError::Io {
                operation: "create the staged dump file",
                source,
            }
        })
    }

    pub fn publish(
        mut self,
        metadata: &DumpArtifactMetadata,
        compression: &CompressionMetrics,
    ) -> Result<PublishedDumpArtifact, ArtifactStoreError> {
        if metadata.dump_id != self.dump_id
            || metadata.profile != self.profile
            || metadata.database != self.database
        {
            return Err(ArtifactStoreError::MetadataIdentityMismatch);
        }
        if metadata.uncompressed_bytes != compression.input_bytes()
            || metadata.compressed_bytes != compression.compressed_bytes()
            || metadata.sql_sha256 != compression.input_sha256()
            || metadata.artifact_sha256 != compression.compressed_sha256()
        {
            return Err(ArtifactStoreError::MetadataContentMismatch);
        }

        let dump_path = self.stage_path.join(DUMP_FILE_NAME);
        let dump_file = OpenOptions::new()
            .write(true)
            .open(&dump_path)
            .map_err(|source| ArtifactStoreError::Io {
                operation: "open the completed staged dump",
                source,
            })?;
        let actual_size = dump_file
            .metadata()
            .and_then(|file_metadata| {
                dump_file.sync_all()?;
                Ok(file_metadata.len())
            })
            .map_err(|source| ArtifactStoreError::Io {
                operation: "sync the completed staged dump",
                source,
            })?;
        if actual_size != metadata.compressed_bytes {
            return Err(ArtifactStoreError::CompressedSizeMismatch {
                expected: metadata.compressed_bytes,
                actual: actual_size,
            });
        }

        write_metadata(&self.stage_path, metadata)?;
        sync_directory(&self.stage_path)?;
        fs::rename(&self.stage_path, &self.published_path).map_err(|source| {
            ArtifactStoreError::Io {
                operation: "publish the completed artifact directory",
                source,
            }
        })?;
        self.published = true;
        drop(self.activity_lock.take());
        sync_directory(&self.parent)?;

        Ok(PublishedDumpArtifact {
            dump_id: self.dump_id,
            path: self.published_path.clone(),
            dump_path: self.published_path.join(DUMP_FILE_NAME),
            metadata_path: self.published_path.join(METADATA_FILE_NAME),
        })
    }

    #[cfg(test)]
    pub(crate) fn stage_path(&self) -> &Path {
        &self.stage_path
    }
}

impl Drop for StagedDumpArtifact {
    fn drop(&mut self) {
        if !self.published {
            drop(self.activity_lock.take());
            let _ = fs::remove_dir_all(&self.stage_path);
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct PublishedDumpArtifact {
    pub(crate) dump_id: DumpId,
    pub(crate) path: PathBuf,
    pub(crate) dump_path: PathBuf,
    pub(crate) metadata_path: PathBuf,
}

#[derive(Debug, Eq, PartialEq)]
pub struct LocatedDumpArtifact {
    profile: ProfileName,
    database: DatabaseName,
    artifact: PublishedDumpArtifact,
}

impl LocatedDumpArtifact {
    pub fn profile(&self) -> &ProfileName {
        &self.profile
    }

    pub fn database(&self) -> &DatabaseName {
        &self.database
    }

    pub fn artifact(&self) -> &PublishedDumpArtifact {
        &self.artifact
    }

    pub(crate) fn into_parts(self) -> (ProfileName, DatabaseName, PublishedDumpArtifact) {
        (self.profile, self.database, self.artifact)
    }
}

impl PublishedDumpArtifact {
    pub const fn dump_id(&self) -> DumpId {
        self.dump_id
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn dump_path(&self) -> &Path {
        &self.dump_path
    }

    pub fn metadata_path(&self) -> &Path {
        &self.metadata_path
    }
}

#[derive(Debug)]
pub struct ArtifactLease {
    _file: File,
}

#[derive(Debug, Error)]
pub enum ArtifactLeaseError {
    #[error("the local dump artifact no longer exists")]
    NotFound,

    #[error("the local dump artifact is currently being modified")]
    Busy,

    #[error("could not {operation}")]
    Io {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
}

#[derive(Debug, Error)]
pub enum ArtifactStoreError {
    #[error("could not {operation}")]
    Io {
        operation: &'static str,
        #[source]
        source: io::Error,
    },

    #[error("could not serialize dump metadata")]
    SerializeMetadata(#[source] serde_json::Error),

    #[error("staged artifact and metadata identify different dumps")]
    MetadataIdentityMismatch,

    #[error("staged artifact metadata differs from its compression result")]
    MetadataContentMismatch,

    #[error("compressed artifact size differs from metadata: expected {expected}, found {actual}")]
    CompressedSizeMismatch { expected: u64, actual: u64 },

    #[error("the local dump artifact is currently in use")]
    ArtifactBusy,
}

pub(crate) fn try_acquire_exclusive_artifact_lock(
    directory: &Path,
) -> Result<Option<File>, ArtifactStoreError> {
    let file =
        open_artifact_lock_file(&directory.join(ARTIFACT_LOCK_FILE_NAME)).map_err(|source| {
            ArtifactStoreError::Io {
                operation: "open an artifact lock",
                source,
            }
        })?;
    match fs4::FileExt::try_lock(&file) {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(source)) => Err(ArtifactStoreError::Io {
            operation: "acquire an artifact lock",
            source,
        }),
    }
}

fn write_metadata(
    stage_path: &Path,
    metadata: &DumpArtifactMetadata,
) -> Result<(), ArtifactStoreError> {
    let part_path = stage_path.join(METADATA_PART_FILE_NAME);
    let final_path = stage_path.join(METADATA_FILE_NAME);
    let file = create_private_file(&part_path).map_err(|source| ArtifactStoreError::Io {
        operation: "create the staged metadata file",
        source,
    })?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, metadata)
        .map_err(ArtifactStoreError::SerializeMetadata)?;
    writer
        .write_all(b"\n")
        .and_then(|_| writer.flush())
        .map_err(|source| ArtifactStoreError::Io {
            operation: "write the staged metadata file",
            source,
        })?;
    let file = writer
        .into_inner()
        .map_err(|error| ArtifactStoreError::Io {
            operation: "finish the staged metadata file",
            source: error.into_error(),
        })?;
    file.sync_all().map_err(|source| ArtifactStoreError::Io {
        operation: "sync the staged metadata file",
        source,
    })?;
    fs::rename(part_path, final_path).map_err(|source| ArtifactStoreError::Io {
        operation: "finalize the staged metadata file",
        source,
    })
}

fn create_private_directories(root: &Path, leaf: &Path) -> Result<(), ArtifactStoreError> {
    fs::create_dir_all(leaf).map_err(|source| ArtifactStoreError::Io {
        operation: "create private artifact directories",
        source,
    })?;
    for directory in [
        root.to_owned(),
        root.join(PROFILES_DIRECTORY),
        leaf.parent().unwrap_or(leaf).to_owned(),
        leaf.to_owned(),
    ] {
        set_private_directory_permissions(&directory).map_err(|source| ArtifactStoreError::Io {
            operation: "restrict an artifact directory",
            source,
        })?;
    }
    Ok(())
}

fn create_private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    set_private_file_permissions(&file)?;
    Ok(file)
}

fn open_artifact_lock_file(path: &Path) -> io::Result<File> {
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

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), ArtifactStoreError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| ArtifactStoreError::Io {
            operation: "sync an artifact directory",
            source,
        })
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), ArtifactStoreError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Cursor, Write as _},
        sync::Arc,
    };

    use tempfile::tempdir;

    use crate::domain::{
        DatabaseEncoding, DatabaseName, DumpArtifactCompletion, DumpArtifactContext, MysqlVersion,
        Sha256Digest,
    };
    use crate::infrastructure::compression::{NoCompressionProgress, ZstdCompressor};

    use super::*;

    fn profile() -> ProfileName {
        ProfileName::try_from("local-source").unwrap()
    }

    fn database() -> DatabaseName {
        DatabaseName::try_from("acme_production").unwrap()
    }

    fn metadata(dump_id: DumpId, compression: &CompressionMetrics) -> DumpArtifactMetadata {
        DumpArtifactMetadata::try_new(
            dump_id,
            DumpArtifactContext {
                database: database(),
                profile: profile(),
                source_fingerprint: Sha256Digest::from_bytes([1; 32]),
                source_server_uuid: "11111111-1111-4111-8111-111111111111".parse().unwrap(),
                source_version: "8.4.4".parse::<MysqlVersion>().unwrap(),
                client_version: "8.4.4".parse::<MysqlVersion>().unwrap(),
                database_encoding: DatabaseEncoding::try_new(
                    "utf8mb4".to_owned(),
                    "utf8mb4_0900_ai_ci".to_owned(),
                )
                .unwrap(),
                policy_version: 1,
            },
            DumpArtifactCompletion {
                created_at_unix_seconds: 100,
                completed_at_unix_seconds: 101,
                uncompressed_bytes: compression.input_bytes(),
                compressed_bytes: compression.compressed_bytes(),
                sql_sha256: compression.input_sha256(),
                artifact_sha256: compression.compressed_sha256(),
            },
        )
        .unwrap()
    }

    #[tokio::test]
    async fn publishes_the_exact_artifact_produced_by_the_streaming_compressor() {
        let directory = tempdir().unwrap();
        let store = LocalArtifactStore::new(directory.path().join("cache"));
        let stage = store.begin(&profile(), &database()).unwrap();
        let dump_id = stage.dump_id();
        let input = b"CREATE TABLE example (id BIGINT);\n".repeat(1024);
        let metrics = ZstdCompressor::default()
            .compress(
                Cursor::new(input.clone()),
                stage.create_dump_writer().unwrap(),
                Arc::new(NoCompressionProgress),
            )
            .await
            .unwrap();
        let metadata = DumpArtifactMetadata::try_new(
            dump_id,
            DumpArtifactContext {
                database: database(),
                profile: profile(),
                source_fingerprint: Sha256Digest::from_bytes([1; 32]),
                source_server_uuid: "11111111-1111-4111-8111-111111111111".parse().unwrap(),
                source_version: "8.4.4".parse().unwrap(),
                client_version: "8.4.4".parse().unwrap(),
                database_encoding: DatabaseEncoding::try_new(
                    "utf8mb4".to_owned(),
                    "utf8mb4_0900_ai_ci".to_owned(),
                )
                .unwrap(),
                policy_version: 1,
            },
            DumpArtifactCompletion {
                created_at_unix_seconds: 100,
                completed_at_unix_seconds: 101,
                uncompressed_bytes: metrics.input_bytes(),
                compressed_bytes: metrics.compressed_bytes(),
                sql_sha256: metrics.input_sha256(),
                artifact_sha256: metrics.compressed_sha256(),
            },
        )
        .unwrap();

        assert!(
            store
                .list_complete(&profile(), &database())
                .unwrap()
                .is_empty()
        );
        let published = stage.publish(&metadata, &metrics).unwrap();
        let decoded = zstd::stream::decode_all(File::open(&published.dump_path).unwrap()).unwrap();

        assert_eq!(decoded, input);
        assert_eq!(published.dump_id, dump_id);
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(&published.metadata_path).unwrap()).unwrap();
        assert_eq!(json["dump_id"], dump_id.to_string());
        assert_eq!(json["database"], "acme_production");
        assert_eq!(
            store.list_complete(&profile(), &database()).unwrap().len(),
            1
        );
        assert!(!published.path.to_string_lossy().ends_with(PART_SUFFIX));
    }

    #[tokio::test]
    async fn failed_or_dropped_stages_are_not_visible_as_complete_artifacts() {
        let directory = tempdir().unwrap();
        let store = LocalArtifactStore::new(directory.path().join("cache"));
        let stage = store.begin(&profile(), &database()).unwrap();
        let stage_path = stage.stage_path().to_owned();
        let dump_id = stage.dump_id();
        let metrics = ZstdCompressor::default()
            .compress(
                Cursor::new(b"short"),
                stage.create_dump_writer().unwrap(),
                Arc::new(NoCompressionProgress),
            )
            .await
            .unwrap();
        OpenOptions::new()
            .append(true)
            .open(stage.stage_path().join(DUMP_FILE_NAME))
            .unwrap()
            .write_all(b"tamper-after-compression")
            .unwrap();

        let error = stage
            .publish(&metadata(dump_id, &metrics), &metrics)
            .unwrap_err();
        assert!(matches!(
            error,
            ArtifactStoreError::CompressedSizeMismatch { .. }
        ));
        assert!(!stage_path.exists());
        assert!(
            store
                .list_complete(&profile(), &database())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_process_crash_can_leave_only_an_ignored_part_directory() {
        let directory = tempdir().unwrap();
        let store = LocalArtifactStore::new(directory.path().join("cache"));
        let stage = store.begin(&profile(), &database()).unwrap();
        let stage_path = stage.stage_path().to_owned();
        stage
            .create_dump_writer()
            .unwrap()
            .write_all(b"partial")
            .unwrap();

        std::mem::forget(stage);

        assert!(stage_path.is_dir());
        assert!(stage_path.to_string_lossy().ends_with(PART_SUFFIX));
        assert!(
            store
                .list_complete(&profile(), &database())
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn metadata_checksum_mismatch_cannot_publish_an_artifact() {
        let directory = tempdir().unwrap();
        let store = LocalArtifactStore::new(directory.path().join("cache"));
        let stage = store.begin(&profile(), &database()).unwrap();
        let dump_id = stage.dump_id();
        let metrics = ZstdCompressor::default()
            .compress(
                Cursor::new(b"valid stream"),
                stage.create_dump_writer().unwrap(),
                Arc::new(NoCompressionProgress),
            )
            .await
            .unwrap();
        let mut invalid_metadata = metadata(dump_id, &metrics);
        invalid_metadata.artifact_sha256 = Sha256Digest::from_bytes([3; 32]);
        let error = stage.publish(&invalid_metadata, &metrics).unwrap_err();

        assert!(matches!(error, ArtifactStoreError::MetadataContentMismatch));
        assert!(
            store
                .list_complete(&profile(), &database())
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn failed_publish_rename_cleans_the_stage_and_never_exposes_a_complete_dump() {
        let directory = tempdir().unwrap();
        let store = LocalArtifactStore::new(directory.path().join("cache"));
        let stage = store.begin(&profile(), &database()).unwrap();
        let dump_id = stage.dump_id();
        let stage_path = stage.stage_path.clone();
        let collision = stage.published_path.clone();
        let metrics = ZstdCompressor::default()
            .compress(
                Cursor::new(b"valid stream"),
                stage.create_dump_writer().unwrap(),
                Arc::new(NoCompressionProgress),
            )
            .await
            .unwrap();
        fs::create_dir(&collision).unwrap();
        fs::write(collision.join("blocks-directory-replacement"), b"occupied").unwrap();

        let error = stage
            .publish(&metadata(dump_id, &metrics), &metrics)
            .unwrap_err();

        assert!(matches!(
            error,
            ArtifactStoreError::Io {
                operation: "publish the completed artifact directory",
                ..
            }
        ));
        assert!(!stage_path.exists());
        assert!(
            store
                .list_complete(&profile(), &database())
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn cache_directories_and_files_are_private_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().unwrap();
        let root = directory.path().join("cache");
        let store = LocalArtifactStore::new(&root);
        let stage = store.begin(&profile(), &database()).unwrap();
        let dump = stage.create_dump_writer().unwrap();

        assert_eq!(
            fs::metadata(root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(dump.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    }
}
