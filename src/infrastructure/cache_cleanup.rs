use std::{
    fs::{self, File},
    io::{self, BufReader},
    path::{Path, PathBuf},
};

use thiserror::Error;
use uuid::Uuid;

use crate::{
    domain::{DumpArtifactMetadata, DumpId, ProfileName, TenantId, TenantLookup},
    infrastructure::artifact_store::{
        ArtifactStoreError, METADATA_FILE_NAME, PART_SUFFIX, PROFILES_DIRECTORY,
        try_acquire_exclusive_artifact_lock,
    },
    infrastructure::cache::DEFAULT_CACHE_TTL_SECONDS,
};

pub const DEFAULT_PARTIAL_TTL_SECONDS: u64 = 60 * 60;
const MAX_METADATA_BYTES: u64 = 64 * 1024;
const DELETING_MARKER: &str = ".deleting-";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheCleanupPolicy {
    pub now_unix_seconds: u64,
    pub artifact_ttl_seconds: u64,
    pub partial_ttl_seconds: u64,
}

impl CacheCleanupPolicy {
    pub const fn defaults_at(now_unix_seconds: u64) -> Self {
        Self {
            now_unix_seconds,
            artifact_ttl_seconds: DEFAULT_CACHE_TTL_SECONDS,
            partial_ttl_seconds: DEFAULT_PARTIAL_TTL_SECONDS,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CacheCleanupReport {
    pub expired_artifacts_removed: u64,
    pub orphan_partials_removed: u64,
    pub interrupted_deletions_removed: u64,
    pub locked_entries_skipped: u64,
    pub future_entries_skipped: u64,
    pub invalid_entries_skipped: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CachePurgeReport {
    pub artifacts_removed: u64,
    pub locked_entries_skipped: u64,
    pub invalid_entries_skipped: u64,
}

#[derive(Clone, Debug)]
pub struct LocalCacheCleaner {
    cache_root: PathBuf,
}

impl LocalCacheCleaner {
    pub fn new(cache_root: impl Into<PathBuf>) -> Self {
        Self {
            cache_root: cache_root.into(),
        }
    }

    pub fn clean(
        &self,
        policy: CacheCleanupPolicy,
    ) -> Result<CacheCleanupReport, CacheCleanupError> {
        let mut report = CacheCleanupReport::default();
        let profiles = self.cache_root.join(PROFILES_DIRECTORY);
        for profile in read_directories(&profiles)? {
            if !has_valid_name::<ProfileName>(&profile) {
                report.invalid_entries_skipped += 1;
                continue;
            }
            for tenant in read_directories(&profile)? {
                if !has_valid_name::<TenantId>(&tenant) {
                    report.invalid_entries_skipped += 1;
                    continue;
                }
                for entry in read_directories(&tenant)? {
                    self.clean_entry(&entry, policy, &mut report)?;
                }
            }
        }
        Ok(report)
    }

    pub fn purge(
        &self,
        profile: &ProfileName,
        tenant_lookup: &TenantLookup,
    ) -> Result<CachePurgeReport, CacheCleanupError> {
        let mut report = CachePurgeReport::default();
        let profile_path = self
            .cache_root
            .join(PROFILES_DIRECTORY)
            .join(profile.as_str());
        for tenant_path in read_directories(&profile_path)? {
            let Some(tenant_name) = tenant_path.file_name().and_then(|name| name.to_str()) else {
                report.invalid_entries_skipped += 1;
                continue;
            };
            let Ok(tenant_id) = TenantId::try_from(tenant_name) else {
                report.invalid_entries_skipped += 1;
                continue;
            };
            let direct_tenant_match = tenant_id.as_str() == tenant_lookup.as_str();
            for artifact_path in read_directories(&tenant_path)? {
                let Some(artifact_name) = artifact_path.file_name().and_then(|name| name.to_str())
                else {
                    report.invalid_entries_skipped += 1;
                    continue;
                };
                if artifact_name.parse::<DumpId>().is_err() {
                    continue;
                }
                let selected = if direct_tenant_match {
                    true
                } else {
                    match read_metadata(&artifact_path)? {
                        Some(metadata) => {
                            metadata.profile == *profile
                                && metadata.tenant_id == tenant_id
                                && (metadata.tenant_lookup == *tenant_lookup
                                    || metadata.tenant_id.as_str() == tenant_lookup.as_str())
                        }
                        None => {
                            report.invalid_entries_skipped += 1;
                            false
                        }
                    }
                };
                if !selected {
                    continue;
                }
                match isolate_and_remove(&artifact_path)? {
                    RemovalOutcome::Removed => report.artifacts_removed += 1,
                    RemovalOutcome::Locked => report.locked_entries_skipped += 1,
                    RemovalOutcome::Gone => {}
                }
            }
        }
        Ok(report)
    }

    fn clean_entry(
        &self,
        path: &Path,
        policy: CacheCleanupPolicy,
        report: &mut CacheCleanupReport,
    ) -> Result<(), CacheCleanupError> {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            report.invalid_entries_skipped += 1;
            return Ok(());
        };

        if is_interrupted_deletion(name) {
            return remove_if_unlocked(path, RemovalKind::InterruptedDeletion, report);
        }
        if let Some(id) = name.strip_suffix(PART_SUFFIX) {
            if id.parse::<DumpId>().is_err() {
                report.invalid_entries_skipped += 1;
                return Ok(());
            }
            return Self::clean_partial(path, policy, report);
        }
        if name.parse::<DumpId>().is_err() {
            report.invalid_entries_skipped += 1;
            return Ok(());
        }
        Self::clean_complete(path, policy, report)
    }

    fn clean_partial(
        path: &Path,
        policy: CacheCleanupPolicy,
        report: &mut CacheCleanupReport,
    ) -> Result<(), CacheCleanupError> {
        let directory_metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(CacheCleanupError::Io {
                    operation: "inspect a partial cache artifact",
                    source,
                });
            }
        };
        let modified = directory_metadata
            .modified()
            .and_then(|modified| {
                modified
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(io::Error::other)
            })
            .map_err(|source| CacheCleanupError::Io {
                operation: "inspect a partial cache artifact",
                source,
            })?
            .as_secs();
        if modified > policy.now_unix_seconds {
            report.future_entries_skipped += 1;
            return Ok(());
        }
        if policy.now_unix_seconds < modified.saturating_add(policy.partial_ttl_seconds) {
            return Ok(());
        }
        remove_if_unlocked(path, RemovalKind::Partial, report)
    }

    fn clean_complete(
        path: &Path,
        policy: CacheCleanupPolicy,
        report: &mut CacheCleanupReport,
    ) -> Result<(), CacheCleanupError> {
        let metadata = match read_metadata(path)? {
            Some(metadata) => metadata,
            None => {
                report.invalid_entries_skipped += 1;
                return Ok(());
            }
        };
        if metadata.completed_at_unix_seconds > policy.now_unix_seconds {
            report.future_entries_skipped += 1;
            return Ok(());
        }
        let expires_at = metadata
            .completed_at_unix_seconds
            .saturating_add(policy.artifact_ttl_seconds);
        if policy.now_unix_seconds < expires_at {
            return Ok(());
        }
        remove_if_unlocked(path, RemovalKind::ExpiredArtifact, report)
    }
}

#[derive(Clone, Copy)]
enum RemovalKind {
    ExpiredArtifact,
    Partial,
    InterruptedDeletion,
}

fn remove_if_unlocked(
    path: &Path,
    kind: RemovalKind,
    report: &mut CacheCleanupReport,
) -> Result<(), CacheCleanupError> {
    match isolate_and_remove(path)? {
        RemovalOutcome::Removed => match kind {
            RemovalKind::ExpiredArtifact => report.expired_artifacts_removed += 1,
            RemovalKind::Partial => report.orphan_partials_removed += 1,
            RemovalKind::InterruptedDeletion => report.interrupted_deletions_removed += 1,
        },
        RemovalOutcome::Locked => {
            report.locked_entries_skipped += 1;
        }
        RemovalOutcome::Gone => {}
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemovalOutcome {
    Removed,
    Locked,
    Gone,
}

fn isolate_and_remove(path: &Path) -> Result<RemovalOutcome, CacheCleanupError> {
    let lock = match try_acquire_exclusive_artifact_lock(path) {
        Ok(Some(lock)) => lock,
        Ok(None) => return Ok(RemovalOutcome::Locked),
        Err(ArtifactStoreError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            return Ok(RemovalOutcome::Gone);
        }
        Err(error) => return Err(error.into()),
    };
    let deletion_path = deletion_path(path)?;
    match fs::rename(path, &deletion_path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(RemovalOutcome::Gone),
        Err(source) => {
            return Err(CacheCleanupError::Io {
                operation: "isolate a cache artifact for deletion",
                source,
            });
        }
    }
    drop(lock);
    fs::remove_dir_all(&deletion_path).map_err(|source| CacheCleanupError::Io {
        operation: "remove an isolated cache artifact",
        source,
    })?;
    Ok(RemovalOutcome::Removed)
}

fn read_metadata(path: &Path) -> Result<Option<DumpArtifactMetadata>, CacheCleanupError> {
    let path = path.join(METADATA_FILE_NAME);
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(CacheCleanupError::Io {
                operation: "open cache metadata during cleanup",
                source,
            });
        }
    };
    let length = file
        .metadata()
        .map_err(|source| CacheCleanupError::Io {
            operation: "inspect cache metadata during cleanup",
            source,
        })?
        .len();
    if length == 0 || length > MAX_METADATA_BYTES {
        return Ok(None);
    }
    Ok(serde_json::from_reader(BufReader::new(file)).ok())
}

fn read_directories(path: &Path) -> Result<Vec<PathBuf>, CacheCleanupError> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(CacheCleanupError::Io {
                operation: "list cache directories during cleanup",
                source,
            });
        }
    };
    let mut directories = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| CacheCleanupError::Io {
            operation: "read a cache directory entry during cleanup",
            source,
        })?;
        let file_type = entry.file_type().map_err(|source| CacheCleanupError::Io {
            operation: "inspect a cache directory entry during cleanup",
            source,
        })?;
        if file_type.is_dir() {
            directories.push(entry.path());
        }
    }
    Ok(directories)
}

fn has_valid_name<T>(path: &Path) -> bool
where
    for<'a> T: TryFrom<&'a str>,
{
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| T::try_from(name).is_ok())
}

fn is_interrupted_deletion(name: &str) -> bool {
    let Some((original, suffix)) = name.split_once(DELETING_MARKER) else {
        return false;
    };
    !suffix.is_empty()
        && Uuid::parse_str(suffix).is_ok()
        && (original.parse::<DumpId>().is_ok()
            || original
                .strip_suffix(PART_SUFFIX)
                .is_some_and(|id| id.parse::<DumpId>().is_ok()))
}

fn deletion_path(path: &Path) -> Result<PathBuf, CacheCleanupError> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(CacheCleanupError::InvalidManagedPath)?;
    Ok(path.with_file_name(format!("{name}{DELETING_MARKER}{}", Uuid::new_v4())))
}

#[derive(Debug, Error)]
pub enum CacheCleanupError {
    #[error(transparent)]
    ArtifactStore(#[from] ArtifactStoreError),

    #[error("could not {operation}")]
    Io {
        operation: &'static str,
        #[source]
        source: io::Error,
    },

    #[error("managed cache path has an invalid file name")]
    InvalidManagedPath,
}

#[cfg(test)]
mod tests {
    use std::{io::Cursor, sync::Arc};

    use tempfile::tempdir;

    use crate::{
        domain::{
            DatabaseEncoding, DatabaseName, DumpArtifactCompletion, DumpArtifactContext,
            MysqlVersion, ProfileName, Sha256Digest, TenantId, TenantLookup,
        },
        infrastructure::{
            artifact_store::{ARTIFACT_LOCK_FILE_NAME, LocalArtifactStore},
            cache::{CacheLookup, CacheLookupResult, LocalCacheValidator},
            compression::{NoCompressionProgress, ZstdCompressor},
        },
    };

    use super::*;

    fn profile() -> ProfileName {
        ProfileName::try_from("local-source").unwrap()
    }

    fn tenant() -> TenantId {
        TenantId::try_from("salt_sagatec").unwrap()
    }

    async fn publish(
        root: &Path,
        completed_at: u64,
    ) -> crate::infrastructure::artifact_store::PublishedDumpArtifact {
        let store = LocalArtifactStore::new(root);
        let stage = store.begin(&profile(), &tenant()).unwrap();
        let id = stage.dump_id();
        let metrics = ZstdCompressor::default()
            .compress(
                Cursor::new(b"SELECT 1;"),
                stage.create_dump_writer().unwrap(),
                Arc::new(NoCompressionProgress),
            )
            .await
            .unwrap();
        let metadata = DumpArtifactMetadata::try_new(
            id,
            DumpArtifactContext {
                tenant_lookup: TenantLookup::try_from("sagatec").unwrap(),
                tenant_id: tenant(),
                database: DatabaseName::try_from("salt_sagatec").unwrap(),
                profile: profile(),
                source_fingerprint: Sha256Digest::from_bytes([1; 32]),
                source_version: "8.4.4".parse::<MysqlVersion>().unwrap(),
                client_version: "8.4.4".parse::<MysqlVersion>().unwrap(),
                database_encoding: DatabaseEncoding::try_new(
                    "utf8mb4".to_owned(),
                    "utf8mb4_0900_ai_ci".to_owned(),
                )
                .unwrap(),
                local_tenant_features: Default::default(),
                policy_version: 1,
            },
            DumpArtifactCompletion {
                created_at_unix_seconds: completed_at - 1,
                completed_at_unix_seconds: completed_at,
                uncompressed_bytes: metrics.input_bytes(),
                compressed_bytes: metrics.compressed_bytes(),
                sql_sha256: metrics.input_sha256(),
                artifact_sha256: metrics.compressed_sha256(),
            },
        )
        .unwrap();
        stage.publish(&metadata, &metrics).unwrap()
    }

    fn policy(now: u64) -> CacheCleanupPolicy {
        CacheCleanupPolicy {
            now_unix_seconds: now,
            artifact_ttl_seconds: 100,
            partial_ttl_seconds: 100,
        }
    }

    #[tokio::test]
    async fn removes_only_expired_complete_artifacts() {
        let directory = tempdir().unwrap();
        let expired = publish(directory.path(), 100).await;
        let current = publish(directory.path(), 950).await;
        let report = LocalCacheCleaner::new(directory.path())
            .clean(policy(1_000))
            .unwrap();

        assert_eq!(report.expired_artifacts_removed, 1);
        assert!(!expired.path.exists());
        assert!(current.path.exists());
    }

    #[test]
    fn removes_stale_partials_but_preserves_recent_and_locked_ones() {
        let directory = tempdir().unwrap();
        let store = LocalArtifactStore::new(directory.path());
        let locked = store.begin(&profile(), &tenant()).unwrap();
        let locked_path = locked.stage_path().to_owned();
        let parent = locked_path.parent().unwrap();
        let stale = parent.join(format!("{}.part", DumpId::new()));
        fs::create_dir(&stale).unwrap();
        let stale_modified = fs::metadata(&stale)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut cleanup_policy = policy(stale_modified.saturating_add(100));
        cleanup_policy.partial_ttl_seconds = 100;
        let report = LocalCacheCleaner::new(directory.path())
            .clean(cleanup_policy)
            .unwrap();

        assert!(!stale.exists());
        assert!(locked_path.exists());
        assert_eq!(report.orphan_partials_removed, 1);
        assert_eq!(report.locked_entries_skipped, 1);

        let recent = parent.join(format!("{}.part", DumpId::new()));
        fs::create_dir(&recent).unwrap();
        let recent_modified = fs::metadata(&recent)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        cleanup_policy.now_unix_seconds = recent_modified.saturating_add(99);
        let report = LocalCacheCleaner::new(directory.path())
            .clean(cleanup_policy)
            .unwrap();
        assert!(recent.exists());
        assert_eq!(report.orphan_partials_removed, 0);
    }

    #[tokio::test]
    async fn a_validated_cache_hit_prevents_deletion_until_drop() {
        let directory = tempdir().unwrap();
        let artifact = publish(directory.path(), 100).await;
        let profile = profile();
        let tenant = tenant();
        let database = DatabaseName::try_from("salt_sagatec").unwrap();
        let validator = LocalCacheValidator::new(LocalArtifactStore::new(directory.path()));
        let hit = validator
            .lookup(&CacheLookup {
                profile: &profile,
                tenant_id: &tenant,
                database: &database,
                source_fingerprint: Sha256Digest::from_bytes([1; 32]),
                policy_version: 1,
                now_unix_seconds: 150,
                ttl_seconds: 1_000,
                fresh: false,
            })
            .unwrap();
        assert!(matches!(hit, CacheLookupResult::Hit(_)));

        let cleaner = LocalCacheCleaner::new(directory.path());
        let report = cleaner.clean(policy(1_000)).unwrap();
        assert_eq!(report.locked_entries_skipped, 1);
        assert!(artifact.path.exists());

        drop(hit);
        let report = cleaner.clean(policy(1_000)).unwrap();
        assert_eq!(report.expired_artifacts_removed, 1);
        assert!(!artifact.path.exists());
    }

    #[tokio::test]
    async fn purge_resolves_an_alias_and_preserves_a_leased_artifact() {
        let directory = tempdir().unwrap();
        let artifact = publish(directory.path(), 100).await;
        let profile = profile();
        let tenant = tenant();
        let database = DatabaseName::try_from("salt_sagatec").unwrap();
        let validator = LocalCacheValidator::new(LocalArtifactStore::new(directory.path()));
        let hit = validator
            .lookup(&CacheLookup {
                profile: &profile,
                tenant_id: &tenant,
                database: &database,
                source_fingerprint: Sha256Digest::from_bytes([1; 32]),
                policy_version: 1,
                now_unix_seconds: 150,
                ttl_seconds: 1_000,
                fresh: false,
            })
            .unwrap();
        let cleaner = LocalCacheCleaner::new(directory.path());
        let lookup = TenantLookup::try_from("sagatec").unwrap();

        let report = cleaner.purge(&profile, &lookup).unwrap();
        assert_eq!(report.artifacts_removed, 0);
        assert_eq!(report.locked_entries_skipped, 1);
        assert!(artifact.path.exists());

        drop(hit);
        let report = cleaner.purge(&profile, &lookup).unwrap();
        assert_eq!(report.artifacts_removed, 1);
        assert!(!artifact.path.exists());
    }

    #[test]
    fn interrupted_deletion_is_recovered_without_following_symlinks() {
        let directory = tempdir().unwrap();
        let parent = directory.path().join("profiles/local-source/salt_sagatec");
        fs::create_dir_all(&parent).unwrap();
        let deleting = parent.join(format!(
            "{}{DELETING_MARKER}{}",
            DumpId::new(),
            Uuid::new_v4()
        ));
        fs::create_dir(&deleting).unwrap();
        fs::write(deleting.join(ARTIFACT_LOCK_FILE_NAME), b"").unwrap();

        let report = LocalCacheCleaner::new(directory.path())
            .clean(policy(1_000))
            .unwrap();
        assert_eq!(report.interrupted_deletions_removed, 1);
        assert!(!deleting.exists());

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(directory.path(), parent.join("not-a-directory")).unwrap();
            LocalCacheCleaner::new(directory.path())
                .clean(policy(1_000))
                .unwrap();
            assert!(directory.path().exists());
        }
    }

    #[tokio::test]
    async fn future_and_invalid_entries_are_preserved() {
        let directory = tempdir().unwrap();
        let future = publish(directory.path(), 2_000).await;
        let invalid = future
            .path
            .parent()
            .unwrap()
            .join(DumpId::new().to_string());
        fs::create_dir(&invalid).unwrap();
        fs::write(invalid.join(METADATA_FILE_NAME), b"{broken").unwrap();

        let report = LocalCacheCleaner::new(directory.path())
            .clean(policy(1_000))
            .unwrap();
        assert_eq!(report.future_entries_skipped, 1);
        assert_eq!(report.invalid_entries_skipped, 1);
        assert!(future.path.exists());
        assert!(invalid.exists());
    }
}
