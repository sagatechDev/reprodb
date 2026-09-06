use std::{
    fs::{self, File},
    io::{self, BufReader, Read},
};

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    domain::{DatabaseName, DumpArtifactMetadata, ProfileName, Sha256Digest, TenantId},
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
    pub tenant_id: &'a TenantId,
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
            .list_complete(request.profile, request.tenant_id)?;
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
        || &metadata.tenant_id != request.tenant_id
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
    use std::{fs::OpenOptions, io::Write as _, path::Path};

    use tempfile::tempdir;

    use crate::domain::{
        DatabaseEncoding, DumpArtifactCompletion, DumpArtifactContext, DumpId, MysqlVersion,
        TenantLookup,
    };

    use super::*;

    fn profile() -> ProfileName {
        ProfileName::try_from("local-source").unwrap()
    }

    fn tenant_id() -> TenantId {
        TenantId::try_from("salt_sagatec").unwrap()
    }

    fn database() -> DatabaseName {
        DatabaseName::try_from("salt_sagatec").unwrap()
    }

    fn fingerprint(byte: u8) -> Sha256Digest {
        Sha256Digest::from_bytes([byte; 32])
    }

    fn write_artifact(root: &Path, completed_at: u64) -> (DumpId, DumpArtifactMetadata) {
        let dump_id = DumpId::new();
        let dump = zstd::stream::encode_all(&b"SELECT * FROM salt_sagatec;"[..], 1).unwrap();
        let checksum = Sha256Digest::from_bytes(Sha256::digest(&dump).into());
        let metadata = DumpArtifactMetadata::try_new(
            dump_id,
            DumpArtifactContext {
                tenant_lookup: TenantLookup::try_from("sagatec").unwrap(),
                tenant_id: tenant_id(),
                database: database(),
                profile: profile(),
                source_fingerprint: fingerprint(1),
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
            .join(tenant_id().as_str())
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

    fn request<'a>(
        profile: &'a ProfileName,
        tenant: &'a TenantId,
        database: &'a DatabaseName,
    ) -> CacheLookup<'a> {
        CacheLookup {
            profile,
            tenant_id: tenant,
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
        let (profile, tenant, database) = (profile(), tenant_id(), database());

        let result = validator
            .lookup(&request(&profile, &tenant, &database))
            .unwrap();

        let CacheLookupResult::Hit(hit) = result else {
            panic!("expected hit")
        };
        assert_eq!(hit.artifact().dump_id(), dump_id);
        assert_eq!(hit.age_seconds(), 100);
    }

    #[test]
    fn fresh_bypasses_cache_without_reading_the_filesystem() {
        let directory = tempdir().unwrap();
        let validator =
            LocalCacheValidator::new(LocalArtifactStore::new(directory.path().join("missing")));
        let (profile, tenant, database) = (profile(), tenant_id(), database());
        let mut lookup = request(&profile, &tenant, &database);
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
        let (profile, tenant, database) = (profile(), tenant_id(), database());
        let mut lookup = request(&profile, &tenant, &database);
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
        let (profile, tenant, database) = (profile(), tenant_id(), database());

        let mut changed = request(&profile, &tenant, &database);
        changed.source_fingerprint = fingerprint(9);
        assert_miss(
            validator.lookup(&changed).unwrap(),
            CacheMissReason::SourceChanged,
        );

        let mut changed = request(&profile, &tenant, &database);
        changed.policy_version = 2;
        assert_miss(
            validator.lookup(&changed).unwrap(),
            CacheMissReason::PolicyChanged,
        );

        let other_database = DatabaseName::try_from("salt_polymer").unwrap();
        let changed = request(&profile, &tenant, &other_database);
        assert_miss(
            validator.lookup(&changed).unwrap(),
            CacheMissReason::IdentityChanged,
        );
    }

    #[test]
    fn an_invalid_newer_artifact_falls_back_to_an_older_valid_one() {
        let directory = tempdir().unwrap();
        let (older_id, _) = write_artifact(directory.path(), 9_800);
        let (newer_id, newer_metadata) = write_artifact(directory.path(), 9_900);
        let newer_path = directory
            .path()
            .join("profiles/local-source/salt_sagatec")
            .join(newer_id.to_string())
            .join("dump.sql.zst");
        fs::write(
            newer_path,
            vec![b'x'; newer_metadata.compressed_bytes as usize],
        )
        .unwrap();

        let validator = LocalCacheValidator::new(LocalArtifactStore::new(directory.path()));
        let (profile, tenant, database) = (profile(), tenant_id(), database());
        let CacheLookupResult::Hit(hit) = validator
            .lookup(&request(&profile, &tenant, &database))
            .unwrap()
        else {
            panic!("expected the older artifact to be reused")
        };

        assert_eq!(hit.artifact().dump_id(), older_id);
        assert_eq!(hit.age_seconds(), 200);
    }

    #[test]
    fn corrupt_metadata_missing_file_size_and_checksum_are_not_hits() {
        let (profile, tenant, database) = (profile(), tenant_id(), database());
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
                .join("profiles/local-source/salt_sagatec")
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
                validator
                    .lookup(&request(&profile, &tenant, &database))
                    .unwrap(),
                expected,
            );
        }
    }
}
