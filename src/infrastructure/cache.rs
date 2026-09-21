use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, BufReader, Read},
};

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    domain::{DatabaseName, DumpArtifactMetadata, ProfileName, Sha256Digest},
    infrastructure::artifact_store::{
        ArtifactLease, ArtifactLeaseError, ArtifactStoreError, LocalArtifactStore,
        PublishedDumpArtifact,
    },
};

pub const DEFAULT_CACHE_TTL_SECONDS: u64 = 2 * 60 * 60;
const MAX_METADATA_BYTES: u64 = 64 * 1024;
const CHECKSUM_BUFFER_BYTES: usize = 64 * 1024;

pub struct CacheLookup<'a> {
    pub profile: &'a ProfileName,
    pub database: &'a DatabaseName,
    pub source_fingerprint: Sha256Digest,
    pub policy_version: u32,
    pub now_unix_seconds: u64,
    pub ttl_seconds: u64,
    pub fresh: bool,
}

pub struct CacheDatabaseLookup<'a> {
    pub profile: &'a ProfileName,
    pub database: &'a DatabaseName,
    pub source_fingerprint: Sha256Digest,
    pub policy_version: u32,
    pub now_unix_seconds: u64,
    pub ttl_seconds: u64,
    pub fresh: bool,
}

#[derive(Debug)]
pub struct ValidatedCacheHit {
    artifact: PublishedDumpArtifact,
    metadata: DumpArtifactMetadata,
    age_seconds: u64,
    _lease: ArtifactLease,
}

impl ValidatedCacheHit {
    pub fn artifact(&self) -> &PublishedDumpArtifact {
        &self.artifact
    }

    pub fn metadata(&self) -> &DumpArtifactMetadata {
        &self.metadata
    }

    pub const fn age_seconds(&self) -> u64 {
        self.age_seconds
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheMissReason {
    FreshRequested,
    NotFound,
    CorruptMetadata,
    IdentityChanged,
    SourceChanged,
    PolicyChanged,
    ClockInFuture,
    Expired,
    SizeMismatch,
    ChecksumMismatch,
    InUse,
}

#[derive(Debug)]
pub enum CacheLookupResult {
    Hit(Box<ValidatedCacheHit>),
    Miss(CacheMissReason),
}

pub struct CacheInventoryRequest<'a> {
    pub source_fingerprints: &'a BTreeMap<ProfileName, Sha256Digest>,
    pub policy_version: u32,
    pub now_unix_seconds: u64,
    pub ttl_seconds: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheEntryStatus {
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CacheInventoryEntry {
    pub profile: ProfileName,
    pub database: DatabaseName,
    pub dump_id: crate::domain::DumpId,
    pub completed_at_unix_seconds: Option<u64>,
    pub expires_at_unix_seconds: Option<u64>,
    pub compressed_bytes: Option<u64>,
    pub status: CacheEntryStatus,
}

pub struct LocalCacheValidator {
    store: LocalArtifactStore,
}

impl LocalCacheValidator {
    pub fn new(store: LocalArtifactStore) -> Self {
        Self { store }
    }

    pub fn lookup(&self, request: &CacheLookup<'_>) -> Result<CacheLookupResult, CacheError> {
        if request.fresh {
            return Ok(CacheLookupResult::Miss(CacheMissReason::FreshRequested));
        }

        let artifacts = self
            .store
            .list_complete(request.profile, request.database)?;
        if artifacts.is_empty() {
            return Ok(CacheLookupResult::Miss(CacheMissReason::NotFound));
        }

        let mut candidates = Vec::new();
        let mut unreadable_metadata = None;
        for artifact in artifacts {
            let lease = match self.store.try_acquire_lease(&artifact) {
                Ok(lease) => lease,
                Err(ArtifactLeaseError::NotFound) => {
                    unreadable_metadata.get_or_insert(CacheMissReason::NotFound);
                    continue;
                }
                Err(ArtifactLeaseError::Busy) => {
                    unreadable_metadata.get_or_insert(CacheMissReason::InUse);
                    continue;
                }
                Err(ArtifactLeaseError::Io { source, .. }) => {
                    return Err(CacheError::Io(source));
                }
            };
            match read_metadata(&artifact)? {
                Ok(metadata) => candidates.push((artifact, metadata, lease)),
                Err(reason) => {
                    unreadable_metadata.get_or_insert(reason);
                }
            }
        }
        candidates
            .sort_by_key(|(_, metadata, _)| std::cmp::Reverse(metadata.completed_at_unix_seconds));

        let mut newest_invalid = None;
        for (artifact, metadata, lease) in candidates {
            match validate_candidate(request, artifact, &metadata, lease)? {
                Ok(hit) => return Ok(CacheLookupResult::Hit(Box::new(hit))),
                Err(reason) => {
                    newest_invalid.get_or_insert(reason);
                }
            }
        }
        Ok(CacheLookupResult::Miss(
            newest_invalid
                .or(unreadable_metadata)
                .unwrap_or(CacheMissReason::NotFound),
        ))
    }

    pub fn lookup_by_database(
        &self,
        request: &CacheDatabaseLookup<'_>,
    ) -> Result<CacheLookupResult, CacheError> {
        if request.fresh {
            return Ok(CacheLookupResult::Miss(CacheMissReason::FreshRequested));
        }

        let artifacts = self.store.list_complete_for_profile(request.profile)?;
        if artifacts.is_empty() {
            return Ok(CacheLookupResult::Miss(CacheMissReason::NotFound));
        }
        let mut candidates = Vec::new();
        let mut unreadable_metadata = None;
        for located in artifacts {
            let (profile, database, artifact) = located.into_parts();
            let lease = match self.store.try_acquire_lease(&artifact) {
                Ok(lease) => lease,
                Err(ArtifactLeaseError::NotFound) => {
                    unreadable_metadata.get_or_insert(CacheMissReason::NotFound);
                    continue;
                }
                Err(ArtifactLeaseError::Busy) => {
                    unreadable_metadata.get_or_insert(CacheMissReason::InUse);
                    continue;
                }
                Err(ArtifactLeaseError::Io { source, .. }) => {
                    return Err(CacheError::Io(source));
                }
            };
            let metadata = match read_metadata(&artifact)? {
                Ok(metadata) => metadata,
                Err(reason) => {
                    unreadable_metadata.get_or_insert(reason);
                    continue;
                }
            };
            if profile != *request.profile || database != metadata.database {
                unreadable_metadata.get_or_insert(CacheMissReason::IdentityChanged);
                continue;
            }
            if metadata.database != *request.database {
                continue;
            }
            candidates.push((artifact, metadata, database, lease));
        }
        candidates.sort_by_key(|(_, metadata, _, _)| {
            std::cmp::Reverse(metadata.completed_at_unix_seconds)
        });

        let mut newest_invalid = None;
        for (artifact, metadata, database, lease) in candidates {
            let candidate_request = CacheLookup {
                profile: request.profile,
                database: &database,
                source_fingerprint: request.source_fingerprint,
                policy_version: request.policy_version,
                now_unix_seconds: request.now_unix_seconds,
                ttl_seconds: request.ttl_seconds,
                fresh: false,
            };
            match validate_candidate(&candidate_request, artifact, &metadata, lease)? {
                Ok(hit) => return Ok(CacheLookupResult::Hit(Box::new(hit))),
                Err(reason) => {
                    newest_invalid.get_or_insert(reason);
                }
            }
        }
        Ok(CacheLookupResult::Miss(
            newest_invalid
                .or(unreadable_metadata)
                .unwrap_or(CacheMissReason::NotFound),
        ))
    }

    pub fn inspect_all(
        &self,
        request: &CacheInventoryRequest<'_>,
    ) -> Result<Vec<CacheInventoryEntry>, CacheError> {
        let mut entries = Vec::new();
        for located in self.store.list_all_candidates()? {
            let (profile, database, artifact) = located.into_parts();
            let dump_id = artifact.dump_id;
            let lease = match self.store.try_acquire_lease(&artifact) {
                Ok(lease) => lease,
                Err(ArtifactLeaseError::NotFound) => {
                    entries.push(CacheInventoryEntry {
                        profile,
                        database,
                        dump_id,
                        completed_at_unix_seconds: None,
                        expires_at_unix_seconds: None,
                        compressed_bytes: None,
                        status: CacheEntryStatus::MissingFile,
                    });
                    continue;
                }
                Err(ArtifactLeaseError::Busy) => {
                    entries.push(CacheInventoryEntry {
                        profile,
                        database,
                        dump_id,
                        completed_at_unix_seconds: None,
                        expires_at_unix_seconds: None,
                        compressed_bytes: None,
                        status: CacheEntryStatus::InUse,
                    });
                    continue;
                }
                Err(ArtifactLeaseError::Io { source, .. }) => {
                    return Err(CacheError::Io(source));
                }
            };
            let metadata = match read_metadata(&artifact)? {
                Ok(metadata) => metadata,
                Err(reason) => {
                    entries.push(CacheInventoryEntry {
                        profile,
                        database,
                        dump_id,
                        completed_at_unix_seconds: None,
                        expires_at_unix_seconds: None,
                        compressed_bytes: file_size(&artifact.dump_path)?,
                        status: if reason == CacheMissReason::NotFound {
                            CacheEntryStatus::MissingFile
                        } else {
                            CacheEntryStatus::CorruptMetadata
                        },
                    });
                    continue;
                }
            };
            let actual_size = file_size(&artifact.dump_path)?;
            let expires_at = metadata
                .completed_at_unix_seconds
                .saturating_add(request.ttl_seconds);
            let status = if metadata.dump_id != dump_id
                || metadata.profile != profile
                || metadata.database != database
            {
                CacheEntryStatus::IdentityChanged
            } else if actual_size.is_none() {
                CacheEntryStatus::MissingFile
            } else if actual_size != Some(metadata.compressed_bytes) {
                CacheEntryStatus::SizeMismatch
            } else if hash_file(&artifact.dump_path)? != Some(metadata.artifact_sha256) {
                CacheEntryStatus::ChecksumMismatch
            } else if metadata.policy_version != request.policy_version {
                CacheEntryStatus::PolicyChanged
            } else if let Some(fingerprint) = request.source_fingerprints.get(&profile) {
                if metadata.source_fingerprint != *fingerprint {
                    CacheEntryStatus::SourceChanged
                } else if metadata.completed_at_unix_seconds > request.now_unix_seconds {
                    CacheEntryStatus::ClockInFuture
                } else if request.now_unix_seconds >= expires_at {
                    CacheEntryStatus::Expired
                } else {
                    CacheEntryStatus::Ready
                }
            } else {
                CacheEntryStatus::ProfileMissing
            };
            entries.push(CacheInventoryEntry {
                profile,
                database,
                dump_id,
                completed_at_unix_seconds: Some(metadata.completed_at_unix_seconds),
                expires_at_unix_seconds: Some(expires_at),
                compressed_bytes: actual_size,
                status,
            });
            drop(lease);
        }
        entries.sort_by(|left, right| {
            right
                .completed_at_unix_seconds
                .cmp(&left.completed_at_unix_seconds)
                .then_with(|| left.profile.cmp(&right.profile))
                .then_with(|| left.database.cmp(&right.database))
                .then_with(|| left.dump_id.to_string().cmp(&right.dump_id.to_string()))
        });
        Ok(entries)
    }
}

#[derive(Debug, Error)]
pub enum CacheError {
    #[error(transparent)]
    Store(#[from] ArtifactStoreError),

    #[error("could not read a local cache artifact")]
    Io(#[source] io::Error),
}

fn read_metadata(
    artifact: &PublishedDumpArtifact,
) -> Result<Result<DumpArtifactMetadata, CacheMissReason>, CacheError> {
    let file = match File::open(&artifact.metadata_path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Err(CacheMissReason::NotFound));
        }
        Err(error) => return Err(CacheError::Io(error)),
    };
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Err(CacheMissReason::NotFound));
        }
        Err(error) => return Err(CacheError::Io(error)),
    };
    if metadata.len() == 0 || metadata.len() > MAX_METADATA_BYTES {
        return Ok(Err(CacheMissReason::CorruptMetadata));
    }
    Ok(serde_json::from_reader(BufReader::new(file)).map_err(|_| CacheMissReason::CorruptMetadata))
}

fn validate_candidate(
    request: &CacheLookup<'_>,
    artifact: PublishedDumpArtifact,
    metadata: &DumpArtifactMetadata,
    lease: ArtifactLease,
) -> Result<Result<ValidatedCacheHit, CacheMissReason>, CacheError> {
    if metadata.dump_id != artifact.dump_id
        || &metadata.profile != request.profile
        || &metadata.database != request.database
        || &metadata.database != request.database
    {
        return Ok(Err(CacheMissReason::IdentityChanged));
    }
    if metadata.source_fingerprint != request.source_fingerprint {
        return Ok(Err(CacheMissReason::SourceChanged));
    }
    if metadata.policy_version != request.policy_version {
        return Ok(Err(CacheMissReason::PolicyChanged));
    }
    if metadata.completed_at_unix_seconds > request.now_unix_seconds {
        return Ok(Err(CacheMissReason::ClockInFuture));
    }
    let expires_at = metadata
        .completed_at_unix_seconds
        .saturating_add(request.ttl_seconds);
    if request.now_unix_seconds >= expires_at {
        return Ok(Err(CacheMissReason::Expired));
    }

    let Some(actual_size) = file_size(&artifact.dump_path)? else {
        return Ok(Err(CacheMissReason::NotFound));
    };
    if actual_size != metadata.compressed_bytes {
        return Ok(Err(CacheMissReason::SizeMismatch));
    }
    let Some(actual_checksum) = hash_file(&artifact.dump_path)? else {
        return Ok(Err(CacheMissReason::NotFound));
    };
    if actual_checksum != metadata.artifact_sha256 {
        return Ok(Err(CacheMissReason::ChecksumMismatch));
    }

    Ok(Ok(ValidatedCacheHit {
        artifact,
        metadata: metadata.clone(),
        age_seconds: request.now_unix_seconds - metadata.completed_at_unix_seconds,
        _lease: lease,
    }))
}

fn file_size(path: &std::path::Path) -> Result<Option<u64>, CacheError> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata.len())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(CacheError::Io(error)),
    }
}

fn hash_file(path: &std::path::Path) -> Result<Option<Sha256Digest>, CacheError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(CacheError::Io(error)),
    };
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; CHECKSUM_BUFFER_BYTES];
    loop {
        let bytes_read = reader.read(&mut buffer).map_err(CacheError::Io)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    Ok(Some(Sha256Digest::from_bytes(hasher.finalize().into())))
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{self, OpenOptions},
        io::Write as _,
        path::Path,
    };

    use tempfile::tempdir;

    use crate::domain::{
        DatabaseEncoding, DumpArtifactCompletion, DumpArtifactContext, DumpId, MysqlVersion,
    };

    use super::*;

    fn profile() -> ProfileName {
        ProfileName::try_from("local-source").unwrap()
    }

    fn database() -> DatabaseName {
        DatabaseName::try_from("acme_production").unwrap()
    }

    fn fingerprint(byte: u8) -> Sha256Digest {
        Sha256Digest::from_bytes([byte; 32])
    }

    fn write_artifact(root: &Path, completed_at: u64) -> (DumpId, DumpArtifactMetadata) {
        let dump_id = DumpId::new();
        let dump = zstd::stream::encode_all(&b"SELECT * FROM acme_production;"[..], 1).unwrap();
        let checksum = Sha256Digest::from_bytes(Sha256::digest(&dump).into());
        let metadata = DumpArtifactMetadata::try_new(
            dump_id,
            DumpArtifactContext {
                database: database(),
                profile: profile(),
                source_fingerprint: fingerprint(1),
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
                created_at_unix_seconds: completed_at - 1,
                completed_at_unix_seconds: completed_at,
                uncompressed_bytes: 28,
                compressed_bytes: dump.len() as u64,
                sql_sha256: fingerprint(2),
                artifact_sha256: checksum,
            },
        )
        .unwrap();
        let path = root
            .join("profiles")
            .join(profile().as_str())
            .join(database().as_str())
            .join(dump_id.to_string());
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("dump.sql.zst"), dump).unwrap();
        fs::write(
            path.join("metadata.json"),
            serde_json::to_vec_pretty(&metadata).unwrap(),
        )
        .unwrap();
        (dump_id, metadata)
    }

    fn request<'a>(profile: &'a ProfileName, database: &'a DatabaseName) -> CacheLookup<'a> {
        CacheLookup {
            profile,
            database,
            source_fingerprint: fingerprint(1),
            policy_version: 1,
            now_unix_seconds: 10_000,
            ttl_seconds: DEFAULT_CACHE_TTL_SECONDS,
            fresh: false,
        }
    }

    fn assert_miss(result: CacheLookupResult, expected: CacheMissReason) {
        let CacheLookupResult::Miss(actual) = result else {
            panic!("expected cache miss")
        };
        assert_eq!(actual, expected);
    }

    #[test]
    fn valid_artifact_is_a_hit_with_age() {
        let directory = tempdir().unwrap();
        let (dump_id, _) = write_artifact(directory.path(), 9_900);
        let validator = LocalCacheValidator::new(LocalArtifactStore::new(directory.path()));
        let (profile, database) = (profile(), database());

        let result = validator.lookup(&request(&profile, &database)).unwrap();

        let CacheLookupResult::Hit(hit) = result else {
            panic!("expected hit")
        };
        assert_eq!(hit.artifact().dump_id(), dump_id);
        assert_eq!(hit.age_seconds(), 100);
    }

    #[test]
    fn lookup_finds_the_artifact_by_its_database_name() {
        let directory = tempdir().unwrap();
        let (dump_id, _) = write_artifact(directory.path(), 9_900);
        let validator = LocalCacheValidator::new(LocalArtifactStore::new(directory.path()));
        let profile = profile();

        let result = validator
            .lookup_by_database(&CacheDatabaseLookup {
                profile: &profile,
                database: &database(),
                source_fingerprint: fingerprint(1),
                policy_version: 1,
                now_unix_seconds: 10_000,
                ttl_seconds: DEFAULT_CACHE_TTL_SECONDS,
                fresh: false,
            })
            .unwrap();
        let CacheLookupResult::Hit(hit) = result else {
            panic!("expected a cache hit for the stored database")
        };
        assert_eq!(hit.metadata().dump_id, dump_id);
        assert_eq!(hit.age_seconds(), 100);

        assert_miss(
            validator
                .lookup_by_database(&CacheDatabaseLookup {
                    profile: &profile,
                    database: &DatabaseName::try_from("globex_production").unwrap(),
                    source_fingerprint: fingerprint(1),
                    policy_version: 1,
                    now_unix_seconds: 10_000,
                    ttl_seconds: DEFAULT_CACHE_TTL_SECONDS,
                    fresh: false,
                })
                .unwrap(),
            CacheMissReason::NotFound,
        );
    }

    #[test]
    fn inventory_verifies_integrity_and_reports_freshness_for_every_profile() {
        let directory = tempdir().unwrap();
        let (dump_id, _) = write_artifact(directory.path(), 9_900);
        let profile = profile();
        let fingerprints = BTreeMap::from([(profile.clone(), fingerprint(1))]);
        let validator = LocalCacheValidator::new(LocalArtifactStore::new(directory.path()));

        let incomplete_id = DumpId::new();
        let incomplete = directory
            .path()
            .join("profiles/local-source/acme_production")
            .join(incomplete_id.to_string());
        fs::create_dir(&incomplete).unwrap();
        fs::write(incomplete.join("dump.sql.zst"), b"incomplete").unwrap();

        let ready = validator
            .inspect_all(&CacheInventoryRequest {
                source_fingerprints: &fingerprints,
                policy_version: 1,
                now_unix_seconds: 10_000,
                ttl_seconds: 200,
            })
            .unwrap();
        assert_eq!(ready.len(), 2);
        assert_eq!(ready[0].dump_id, dump_id);
        assert_eq!(ready[0].status, CacheEntryStatus::Ready);
        assert_eq!(ready[0].database.as_str(), "acme_production");
        assert_eq!(ready[0].expires_at_unix_seconds, Some(10_100));
        assert!(ready.iter().any(|entry| {
            entry.dump_id == incomplete_id && entry.status == CacheEntryStatus::MissingFile
        }));

        let expired = validator
            .inspect_all(&CacheInventoryRequest {
                source_fingerprints: &fingerprints,
                policy_version: 1,
                now_unix_seconds: 10_100,
                ttl_seconds: 200,
            })
            .unwrap();
        assert_eq!(expired[0].status, CacheEntryStatus::Expired);

        let dump_path = directory
            .path()
            .join("profiles/local-source/acme_production")
            .join(dump_id.to_string())
            .join("dump.sql.zst");
        let mut bytes = fs::read(&dump_path).unwrap();
        bytes[0] ^= 0xff;
        fs::write(dump_path, bytes).unwrap();
        let corrupt = validator
            .inspect_all(&CacheInventoryRequest {
                source_fingerprints: &fingerprints,
                policy_version: 1,
                now_unix_seconds: 10_000,
                ttl_seconds: 200,
            })
            .unwrap();
        assert_eq!(corrupt[0].status, CacheEntryStatus::ChecksumMismatch);
    }

    #[test]
    fn fresh_bypasses_cache_without_reading_the_filesystem() {
        let directory = tempdir().unwrap();
        let validator =
            LocalCacheValidator::new(LocalArtifactStore::new(directory.path().join("missing")));
        let (profile, database) = (profile(), database());
        let mut lookup = request(&profile, &database);
        lookup.fresh = true;

        assert_miss(
            validator.lookup(&lookup).unwrap(),
            CacheMissReason::FreshRequested,
        );
    }

    #[test]
    fn ttl_and_future_clock_are_rejected_at_the_boundary() {
        let directory = tempdir().unwrap();
        write_artifact(directory.path(), 9_900);
        let validator = LocalCacheValidator::new(LocalArtifactStore::new(directory.path()));
        let (profile, database) = (profile(), database());
        let mut lookup = request(&profile, &database);
        lookup.ttl_seconds = 100;
        assert_miss(validator.lookup(&lookup).unwrap(), CacheMissReason::Expired);

        lookup.now_unix_seconds = 9_899;
        assert_miss(
            validator.lookup(&lookup).unwrap(),
            CacheMissReason::ClockInFuture,
        );
    }

    #[test]
    fn identity_fingerprint_and_policy_changes_invalidate_cache() {
        let directory = tempdir().unwrap();
        write_artifact(directory.path(), 9_900);
        let validator = LocalCacheValidator::new(LocalArtifactStore::new(directory.path()));
        let (profile, database) = (profile(), database());

        let mut changed = request(&profile, &database);
        changed.source_fingerprint = fingerprint(9);
        assert_miss(
            validator.lookup(&changed).unwrap(),
            CacheMissReason::SourceChanged,
        );

        let mut changed = request(&profile, &database);
        changed.policy_version = 2;
        assert_miss(
            validator.lookup(&changed).unwrap(),
            CacheMissReason::PolicyChanged,
        );
    }

    #[test]
    fn an_invalid_newer_artifact_falls_back_to_an_older_valid_one() {
        let directory = tempdir().unwrap();
        let (older_id, _) = write_artifact(directory.path(), 9_800);
        let (newer_id, newer_metadata) = write_artifact(directory.path(), 9_900);
        let newer_path = directory
            .path()
            .join("profiles/local-source/acme_production")
            .join(newer_id.to_string())
            .join("dump.sql.zst");
        fs::write(
            newer_path,
            vec![b'x'; newer_metadata.compressed_bytes as usize],
        )
        .unwrap();

        let validator = LocalCacheValidator::new(LocalArtifactStore::new(directory.path()));
        let (profile, database) = (profile(), database());
        let CacheLookupResult::Hit(hit) = validator.lookup(&request(&profile, &database)).unwrap()
        else {
            panic!("expected the older artifact to be reused")
        };

        assert_eq!(hit.artifact().dump_id(), older_id);
        assert_eq!(hit.age_seconds(), 200);
    }

    #[test]
    fn corrupt_metadata_missing_file_size_and_checksum_are_not_hits() {
        let (profile, database) = (profile(), database());
        for (mutation, expected) in [
            ("metadata", CacheMissReason::CorruptMetadata),
            ("identity", CacheMissReason::IdentityChanged),
            ("missing", CacheMissReason::NotFound),
            ("size", CacheMissReason::SizeMismatch),
            ("checksum", CacheMissReason::ChecksumMismatch),
        ] {
            let directory = tempdir().unwrap();
            let (dump_id, metadata) = write_artifact(directory.path(), 9_900);
            let path = directory
                .path()
                .join("profiles/local-source/acme_production")
                .join(dump_id.to_string());
            match mutation {
                "metadata" => fs::write(path.join("metadata.json"), b"{broken").unwrap(),
                "identity" => {
                    let mut changed = metadata.clone();
                    changed.profile = ProfileName::try_from("other-source").unwrap();
                    fs::write(
                        path.join("metadata.json"),
                        serde_json::to_vec_pretty(&changed).unwrap(),
                    )
                    .unwrap();
                }
                "missing" => fs::remove_file(path.join("dump.sql.zst")).unwrap(),
                "size" => OpenOptions::new()
                    .append(true)
                    .open(path.join("dump.sql.zst"))
                    .unwrap()
                    .write_all(b"x")
                    .unwrap(),
                "checksum" => {
                    let replacement = vec![b'x'; metadata.compressed_bytes as usize];
                    fs::write(path.join("dump.sql.zst"), replacement).unwrap();
                }
                _ => unreachable!(),
            }
            let validator = LocalCacheValidator::new(LocalArtifactStore::new(directory.path()));
            assert_miss(
                validator.lookup(&request(&profile, &database)).unwrap(),
                expected,
            );
        }
    }
}
