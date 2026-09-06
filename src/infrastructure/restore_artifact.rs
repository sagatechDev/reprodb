use std::{
    fs::File,
    io::{self, BufReader, Read},
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::task::JoinError;

use crate::{
    domain::{DumpArtifactMetadata, DumpId, ProfileName, Sha256Digest, TenantId},
    infrastructure::artifact_store::{
        ArtifactLease, ArtifactLeaseError, ArtifactStoreError, LocalArtifactStore,
        PublishedDumpArtifact,
    },
};

const MAX_METADATA_BYTES: u64 = 64 * 1024;
const VALIDATION_BUFFER_BYTES: usize = 64 * 1024;

pub struct RestoreArtifactRequest<'a> {
    pub profile: &'a ProfileName,
    pub tenant_id: &'a TenantId,
    pub dump_id: DumpId,
}

#[derive(Debug)]
pub struct ValidatedRestoreArtifact {
    artifact: PublishedDumpArtifact,
    metadata: DumpArtifactMetadata,
    _lease: ArtifactLease,
}

impl ValidatedRestoreArtifact {
    pub fn metadata(&self) -> &DumpArtifactMetadata {
        &self.metadata
    }

    pub fn dump_path(&self) -> &Path {
        self.artifact.dump_path()
    }
}

#[derive(Clone, Debug)]
pub struct LocalRestoreArtifactValidator {
    store: LocalArtifactStore,
}

impl LocalRestoreArtifactValidator {
    pub fn new(cache_root: impl Into<PathBuf>) -> Self {
        Self {
            store: LocalArtifactStore::new(cache_root),
        }
    }

    pub async fn validate(
        &self,
        request: RestoreArtifactRequest<'_>,
    ) -> Result<ValidatedRestoreArtifact, RestoreArtifactError> {
        let store = self.store.clone();
        let profile = request.profile.clone();
        let tenant_id = request.tenant_id.clone();
        let dump_id = request.dump_id;
        tokio::task::spawn_blocking(move || validate_blocking(store, profile, tenant_id, dump_id))
            .await
            .map_err(RestoreArtifactError::ValidationTask)?
    }
}

fn validate_blocking(
    store: LocalArtifactStore,
    profile: ProfileName,
    tenant_id: TenantId,
    dump_id: DumpId,
) -> Result<ValidatedRestoreArtifact, RestoreArtifactError> {
    let artifact = store
        .list_complete(&profile, &tenant_id)?
        .into_iter()
        .find(|artifact| artifact.dump_id() == dump_id)
        .ok_or(RestoreArtifactError::NotFound)?;
    let lease = store.try_acquire_lease(&artifact)?;
    let metadata = read_metadata(artifact.metadata_path())?;
    if metadata.dump_id != dump_id || metadata.profile != profile || metadata.tenant_id != tenant_id
    {
        return Err(RestoreArtifactError::IdentityMismatch);
    }

    let actual_size = artifact
        .dump_path()
        .metadata()
        .map_err(RestoreArtifactError::ReadArtifact)?
        .len();
    if actual_size != metadata.compressed_bytes {
        return Err(RestoreArtifactError::CompressedSizeMismatch);
    }
    validate_zstd_and_checksums(artifact.dump_path(), &metadata)?;

    Ok(ValidatedRestoreArtifact {
        artifact,
        metadata,
        _lease: lease,
    })
}

fn read_metadata(path: &Path) -> Result<DumpArtifactMetadata, RestoreArtifactError> {
    let file = File::open(path).map_err(RestoreArtifactError::ReadMetadata)?;
    let size = file
        .metadata()
        .map_err(RestoreArtifactError::ReadMetadata)?
        .len();
    if size == 0 || size > MAX_METADATA_BYTES {
        return Err(RestoreArtifactError::InvalidMetadata);
    }
    serde_json::from_reader(BufReader::new(file)).map_err(|_| RestoreArtifactError::InvalidMetadata)
}

fn validate_zstd_and_checksums(
    path: &Path,
    metadata: &DumpArtifactMetadata,
) -> Result<(), RestoreArtifactError> {
    let file = File::open(path).map_err(RestoreArtifactError::ReadArtifact)?;
    let mut compressed = CountingHashReader::new(BufReader::new(file));
    let mut sql_hasher = Sha256::new();
    let mut sql_bytes = 0_u64;
    {
        let mut decoder = zstd::stream::read::Decoder::new(&mut compressed)
            .map_err(|_| RestoreArtifactError::InvalidZstd)?;
        let mut buffer = [0_u8; VALIDATION_BUFFER_BYTES];
        loop {
            let count = decoder
                .read(&mut buffer)
                .map_err(|_| RestoreArtifactError::InvalidZstd)?;
            if count == 0 {
                break;
            }
            sql_hasher.update(&buffer[..count]);
            sql_bytes = sql_bytes
                .checked_add(count as u64)
                .ok_or(RestoreArtifactError::UncompressedSizeMismatch)?;
        }
    }
    let (compressed_bytes, artifact_sha256) = compressed.finish();
    if compressed_bytes != metadata.compressed_bytes || artifact_sha256 != metadata.artifact_sha256
    {
        return Err(RestoreArtifactError::ArtifactChecksumMismatch);
    }
    if sql_bytes != metadata.uncompressed_bytes {
        return Err(RestoreArtifactError::UncompressedSizeMismatch);
    }
    let sql_sha256 = Sha256Digest::from_bytes(sql_hasher.finalize().into());
    if sql_sha256 != metadata.sql_sha256 {
        return Err(RestoreArtifactError::SqlChecksumMismatch);
    }
    Ok(())
}

struct CountingHashReader<R> {
    inner: R,
    hasher: Sha256,
    bytes: u64,
}

impl<R> CountingHashReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            bytes: 0,
        }
    }

    fn finish(self) -> (u64, Sha256Digest) {
        (
            self.bytes,
            Sha256Digest::from_bytes(self.hasher.finalize().into()),
        )
    }
}

impl<R: Read> Read for CountingHashReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buffer)?;
        self.hasher.update(&buffer[..count]);
        self.bytes = self
            .bytes
            .checked_add(count as u64)
            .ok_or_else(|| io::Error::other("artifact byte counter overflow"))?;
        Ok(count)
    }
}

#[derive(Debug, Error)]
pub enum RestoreArtifactError {
    #[error(transparent)]
    Store(#[from] ArtifactStoreError),

    #[error(transparent)]
    Lease(#[from] ArtifactLeaseError),

    #[error("the managed dump artifact was not found")]
    NotFound,

    #[error("could not read managed dump metadata")]
    ReadMetadata(#[source] io::Error),

    #[error("managed dump metadata is malformed or unsupported")]
    InvalidMetadata,

    #[error("managed dump metadata does not match its profile, tenant or ID")]
    IdentityMismatch,

    #[error("could not read the managed dump artifact")]
    ReadArtifact(#[source] io::Error),

    #[error("managed dump compressed size does not match metadata")]
    CompressedSizeMismatch,

    #[error("managed dump artifact checksum does not match metadata")]
    ArtifactChecksumMismatch,

    #[error("managed dump is not a complete Zstandard stream")]
    InvalidZstd,

    #[error("managed dump uncompressed size does not match metadata")]
    UncompressedSizeMismatch,

    #[error("managed dump SQL checksum does not match metadata")]
    SqlChecksumMismatch,

    #[error("the background artifact validation task terminated unexpectedly")]
    ValidationTask(#[source] JoinError),
}

#[cfg(test)]
mod tests {
    use std::{io::Cursor, sync::Arc};

    use tempfile::tempdir;

    use crate::{
        domain::{
            DatabaseEncoding, DatabaseName, DumpArtifactCompletion, DumpArtifactContext,
            MysqlVersion, TenantLookup,
        },
        infrastructure::{
            artifact_store::LocalArtifactStore,
            compression::{NoCompressionProgress, ZstdCompressor},
        },
    };

    use super::*;

    async fn artifact(root: &Path) -> (ProfileName, TenantId, DumpId, DumpArtifactMetadata) {
        let profile = ProfileName::try_from("local-source").unwrap();
        let tenant = TenantId::try_from("salt_sagatec").unwrap();
        let store = LocalArtifactStore::new(root);
        let stage = store.begin(&profile, &tenant).unwrap();
        let dump_id = stage.dump_id();
        let output = stage.create_dump_writer().unwrap();
        let sql = b"CREATE TABLE `example` (`id` BIGINT);\n";
        let metrics = ZstdCompressor::default()
            .compress(Cursor::new(sql), output, Arc::new(NoCompressionProgress))
            .await
            .unwrap();
        let metadata = DumpArtifactMetadata::try_new(
            dump_id,
            DumpArtifactContext {
                tenant_lookup: TenantLookup::try_from("sagatec").unwrap(),
                tenant_id: tenant.clone(),
                database: DatabaseName::try_from("salt_sagatec").unwrap(),
                profile: profile.clone(),
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
                created_at_unix_seconds: 100,
                completed_at_unix_seconds: 101,
                uncompressed_bytes: metrics.input_bytes(),
                compressed_bytes: metrics.compressed_bytes(),
                sql_sha256: metrics.input_sha256(),
                artifact_sha256: metrics.compressed_sha256(),
            },
        )
        .unwrap();
        stage.publish(&metadata, &metrics).unwrap();
        (profile, tenant, dump_id, metadata)
    }

    #[tokio::test]
    async fn validates_identity_size_both_checksums_and_zstd_before_returning() {
        let directory = tempdir().unwrap();
        let (profile, tenant, dump_id, metadata) = artifact(directory.path()).await;

        let validated = LocalRestoreArtifactValidator::new(directory.path())
            .validate(RestoreArtifactRequest {
                profile: &profile,
                tenant_id: &tenant,
                dump_id,
            })
            .await
            .unwrap();

        assert_eq!(validated.metadata(), &metadata);
        assert!(validated.dump_path().ends_with("dump.sql.zst"));
    }

    #[tokio::test]
    async fn corruption_is_rejected_before_a_validated_artifact_exists() {
        for mutation in ["zstd", "metadata-sql-hash", "metadata-size"] {
            let directory = tempdir().unwrap();
            let (profile, tenant, dump_id, mut metadata) = artifact(directory.path()).await;
            let artifact_dir = directory
                .path()
                .join("profiles/local-source/salt_sagatec")
                .join(dump_id.to_string());
            match mutation {
                "zstd" => {
                    let mut bytes = std::fs::read(artifact_dir.join("dump.sql.zst")).unwrap();
                    bytes[0] ^= 0xff;
                    metadata.artifact_sha256 =
                        Sha256Digest::from_bytes(Sha256::digest(&bytes).into());
                    std::fs::write(artifact_dir.join("dump.sql.zst"), bytes).unwrap();
                }
                "metadata-sql-hash" => {
                    metadata.sql_sha256 = Sha256Digest::from_bytes([9; 32]);
                }
                "metadata-size" => metadata.uncompressed_bytes += 1,
                _ => unreachable!(),
            }
            std::fs::write(
                artifact_dir.join("metadata.json"),
                serde_json::to_vec_pretty(&metadata).unwrap(),
            )
            .unwrap();

            let error = LocalRestoreArtifactValidator::new(directory.path())
                .validate(RestoreArtifactRequest {
                    profile: &profile,
                    tenant_id: &tenant,
                    dump_id,
                })
                .await
                .unwrap_err();

            assert!(matches!(
                error,
                RestoreArtifactError::InvalidZstd
                    | RestoreArtifactError::SqlChecksumMismatch
                    | RestoreArtifactError::UncompressedSizeMismatch
            ));
        }
    }
}
