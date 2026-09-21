use std::{collections::BTreeMap, path::PathBuf};

use thiserror::Error;

use crate::{
    application::{Clock, ClockError, SystemClock},
    domain::{DatabaseName, DumpId, MYSQL_8_DUMP_POLICY_VERSION, ProfileName, Sha256Digest},
    infrastructure::{
        artifact_store::LocalArtifactStore,
        cache::{
            CacheEntryStatus, CacheError, CacheInventoryRequest, DEFAULT_CACHE_TTL_SECONDS,
            LocalCacheValidator,
        },
        cache_cleanup::{
            CacheCleanupError, CacheCleanupPolicy, CacheCleanupReport, CachePurgeReport,
            LocalCacheCleaner,
        },
        config::{ConfigError, ConfigRepository, source_profile_fingerprint},
    },
};

pub struct CacheService<C = SystemClock> {
    repository: ConfigRepository,
    clock: C,
}

impl CacheService<SystemClock> {
    pub fn new(repository: ConfigRepository) -> Self {
        Self {
            repository,
            clock: SystemClock,
        }
    }
}

impl<C> CacheService<C>
where
    C: Clock,
{
    #[cfg(test)]
    fn with_clock(repository: ConfigRepository, clock: C) -> Self {
        Self { repository, clock }
    }

    pub fn list(&self) -> Result<CacheListReport, CacheServiceError> {
        let config = self.repository.load()?;
        let now_unix_seconds = self.clock.now_unix_seconds()?;
        let source_fingerprints: BTreeMap<ProfileName, Sha256Digest> = config
            .profiles
            .iter()
            .map(|(name, profile)| (name.clone(), source_profile_fingerprint(name, profile)))
            .collect();
        let entries =
            LocalCacheValidator::new(LocalArtifactStore::new(self.repository.paths().cache_dir()))
                .inspect_all(&CacheInventoryRequest {
                    source_fingerprints: &source_fingerprints,
                    policy_version: MYSQL_8_DUMP_POLICY_VERSION,
                    now_unix_seconds,
                    ttl_seconds: DEFAULT_CACHE_TTL_SECONDS,
                })?
                .into_iter()
                .map(|entry| CacheListEntry {
                    profile: entry.profile,
                    database: entry.database,
                    dump_id: entry.dump_id,
                    completed_at_unix_seconds: entry.completed_at_unix_seconds,
                    expires_at_unix_seconds: entry.expires_at_unix_seconds,
                    compressed_bytes: entry.compressed_bytes,
                    status: entry.status.into(),
                })
                .collect::<Vec<_>>();
        let total_compressed_bytes = entries
            .iter()
            .filter_map(|entry| entry.compressed_bytes)
            .sum();
        Ok(CacheListReport {
            cache_root: self.repository.paths().cache_dir().to_owned(),
            now_unix_seconds,
            total_compressed_bytes,
            entries,
        })
    }

    pub fn clean(&self) -> Result<CacheCleanupReport, CacheServiceError> {
        let now_unix_seconds = self.clock.now_unix_seconds()?;
        Ok(LocalCacheCleaner::new(self.repository.paths().cache_dir())
            .clean(CacheCleanupPolicy::defaults_at(now_unix_seconds))?)
    }

    pub fn purge(&self, database: &DatabaseName) -> Result<CachePurgeReady, CacheServiceError> {
        let config = self.repository.load()?;
        let profile = config
            .active_profile
            .ok_or(CacheServiceError::NoActiveProfile)?;
        let report = LocalCacheCleaner::new(self.repository.paths().cache_dir())
            .purge(&profile, database)?;
        Ok(CachePurgeReady {
            profile,
            database: database.clone(),
            report,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheListStatus {
    Ready,
    Expired,
    ProfileMissing,
    SourceChanged,
    PolicyChanged,
    ClockInFuture,
    CorruptMetadata,
    IdentityChanged,
    MissingFile,
    SizeMismatch,
    ChecksumMismatch,
    InUse,
}

impl From<CacheEntryStatus> for CacheListStatus {
    fn from(value: CacheEntryStatus) -> Self {
        match value {
            CacheEntryStatus::Ready => Self::Ready,
            CacheEntryStatus::Expired => Self::Expired,
            CacheEntryStatus::ProfileMissing => Self::ProfileMissing,
            CacheEntryStatus::SourceChanged => Self::SourceChanged,
            CacheEntryStatus::PolicyChanged => Self::PolicyChanged,
            CacheEntryStatus::ClockInFuture => Self::ClockInFuture,
            CacheEntryStatus::CorruptMetadata => Self::CorruptMetadata,
            CacheEntryStatus::IdentityChanged => Self::IdentityChanged,
            CacheEntryStatus::MissingFile => Self::MissingFile,
            CacheEntryStatus::SizeMismatch => Self::SizeMismatch,
            CacheEntryStatus::ChecksumMismatch => Self::ChecksumMismatch,
            CacheEntryStatus::InUse => Self::InUse,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CacheListEntry {
    pub profile: ProfileName,
    pub database: DatabaseName,
    pub dump_id: DumpId,
    pub completed_at_unix_seconds: Option<u64>,
    pub expires_at_unix_seconds: Option<u64>,
    pub compressed_bytes: Option<u64>,
    pub status: CacheListStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CacheListReport {
    pub cache_root: PathBuf,
    pub now_unix_seconds: u64,
    pub total_compressed_bytes: u64,
    pub entries: Vec<CacheListEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CachePurgeReady {
    pub profile: ProfileName,
    pub database: DatabaseName,
    pub report: CachePurgeReport,
}

#[derive(Debug, Error)]
pub enum CacheServiceError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error("no active source profile; run `reprodb profile use NAME` first")]
    NoActiveProfile,

    #[error(transparent)]
    Clock(#[from] ClockError),

    #[error(transparent)]
    Cache(#[from] CacheError),

    #[error(transparent)]
    Cleanup(#[from] CacheCleanupError),
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use crate::infrastructure::config::AppPaths;

    use super::*;

    #[derive(Clone, Copy)]
    struct FixedClock(u64);

    impl Clock for FixedClock {
        fn now_unix_seconds(&self) -> Result<u64, ClockError> {
            Ok(self.0)
        }
    }

    fn service(root: &std::path::Path) -> CacheService<FixedClock> {
        CacheService::with_clock(
            ConfigRepository::new(AppPaths::from_root(root)),
            FixedClock(10_000),
        )
    }

    #[test]
    fn an_empty_cache_is_listed_without_requiring_configuration() {
        let directory = tempdir().unwrap();

        let report = service(directory.path()).list().unwrap();

        assert!(report.entries.is_empty());
        assert_eq!(report.total_compressed_bytes, 0);
        assert_eq!(report.now_unix_seconds, 10_000);
        assert_eq!(report.cache_root, directory.path().join("cache"));
    }

    #[test]
    fn clean_is_safe_when_the_cache_does_not_exist() {
        let directory = tempdir().unwrap();

        assert_eq!(
            service(directory.path()).clean().unwrap(),
            CacheCleanupReport::default()
        );
    }

    #[test]
    fn purge_requires_an_active_profile_before_touching_the_cache() {
        let directory = tempdir().unwrap();
        let error = service(directory.path())
            .purge(&DatabaseName::try_from("acme_production").unwrap())
            .unwrap_err();

        assert!(matches!(error, CacheServiceError::NoActiveProfile));
    }
}
