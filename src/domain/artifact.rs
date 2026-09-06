use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;
use uuid::Uuid;

use crate::domain::{
    DatabaseEncoding, DatabaseName, MysqlVersion, ProfileName, Sha256Digest, TenantId, TenantLookup,
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DumpId(Uuid);

impl DumpId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for DumpId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for DumpId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.hyphenated().fmt(formatter)
    }
}

impl FromStr for DumpId {
    type Err = DumpMetadataError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let id = Uuid::parse_str(value).map_err(|_| DumpMetadataError::InvalidDumpId)?;
        if id.hyphenated().to_string() != value {
            return Err(DumpMetadataError::InvalidDumpId);
        }
        Ok(Self(id))
    }
}

impl Serialize for DumpId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for DumpId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DumpArtifactFormat {
    MysqlSqlZstdV1,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DumpArtifactMetadata {
    pub format: DumpArtifactFormat,
    pub dump_id: DumpId,
    pub tenant_lookup: TenantLookup,
    pub tenant_id: TenantId,
    pub database: DatabaseName,
    pub profile: ProfileName,
    pub source_fingerprint: Sha256Digest,
    pub source_version: MysqlVersion,
    pub client_version: MysqlVersion,
    pub database_charset: String,
    pub database_collation: String,
    pub policy_version: u32,
    pub created_at_unix_seconds: u64,
    pub completed_at_unix_seconds: u64,
    pub uncompressed_bytes: u64,
    pub compressed_bytes: u64,
    pub sql_sha256: Sha256Digest,
    pub artifact_sha256: Sha256Digest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DumpArtifactContext {
    pub tenant_lookup: TenantLookup,
    pub tenant_id: TenantId,
    pub database: DatabaseName,
    pub profile: ProfileName,
    pub source_fingerprint: Sha256Digest,
    pub source_version: MysqlVersion,
    pub client_version: MysqlVersion,
    pub database_encoding: DatabaseEncoding,
    pub policy_version: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DumpArtifactCompletion {
    pub created_at_unix_seconds: u64,
    pub completed_at_unix_seconds: u64,
    pub uncompressed_bytes: u64,
    pub compressed_bytes: u64,
    pub sql_sha256: Sha256Digest,
    pub artifact_sha256: Sha256Digest,
}

impl DumpArtifactMetadata {
    pub fn try_new(
        dump_id: DumpId,
        context: DumpArtifactContext,
        completion: DumpArtifactCompletion,
    ) -> Result<Self, DumpMetadataError> {
        if context.policy_version == 0 {
            return Err(DumpMetadataError::InvalidPolicyVersion);
        }
        if completion.completed_at_unix_seconds < completion.created_at_unix_seconds {
            return Err(DumpMetadataError::InvalidTimestamps);
        }
        if completion.compressed_bytes == 0 {
            return Err(DumpMetadataError::EmptyArtifact);
        }

        Ok(Self {
            format: DumpArtifactFormat::MysqlSqlZstdV1,
            dump_id,
            tenant_lookup: context.tenant_lookup,
            tenant_id: context.tenant_id,
            database: context.database,
            profile: context.profile,
            source_fingerprint: context.source_fingerprint,
            source_version: context.source_version,
            client_version: context.client_version,
            database_charset: context.database_encoding.charset().to_owned(),
            database_collation: context.database_encoding.collation().to_owned(),
            policy_version: context.policy_version,
            created_at_unix_seconds: completion.created_at_unix_seconds,
            completed_at_unix_seconds: completion.completed_at_unix_seconds,
            uncompressed_bytes: completion.uncompressed_bytes,
            compressed_bytes: completion.compressed_bytes,
            sql_sha256: completion.sql_sha256,
            artifact_sha256: completion.artifact_sha256,
        })
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum DumpMetadataError {
    #[error("dump ID must be a canonical hyphenated UUID")]
    InvalidDumpId,

    #[error("dump policy version must be greater than zero")]
    InvalidPolicyVersion,

    #[error("dump completion time cannot be before its creation time")]
    InvalidTimestamps,

    #[error("a completed dump artifact cannot be empty")]
    EmptyArtifact,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(byte: u8) -> Sha256Digest {
        Sha256Digest::from_bytes([byte; 32])
    }

    fn metadata(
        completed: u64,
        compressed_bytes: u64,
    ) -> Result<DumpArtifactMetadata, DumpMetadataError> {
        DumpArtifactMetadata::try_new(
            DumpId::new(),
            DumpArtifactContext {
                tenant_lookup: TenantLookup::try_from("sagatec").unwrap(),
                tenant_id: TenantId::try_from("salt_sagatec").unwrap(),
                database: DatabaseName::try_from("salt_sagatec").unwrap(),
                profile: ProfileName::try_from("local-source").unwrap(),
                source_fingerprint: digest(1),
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
                completed_at_unix_seconds: completed,
                uncompressed_bytes: 1024,
                compressed_bytes,
                sql_sha256: digest(2),
                artifact_sha256: digest(3),
            },
        )
    }

    #[test]
    fn metadata_serializes_only_canonical_typed_values() {
        let metadata = metadata(101, 512).unwrap();
        let json = serde_json::to_value(&metadata).unwrap();

        assert_eq!(json["format"], "mysql-sql-zstd-v1");
        assert_eq!(json["tenant_lookup"], "sagatec");
        assert_eq!(json["tenant_id"], "salt_sagatec");
        assert_eq!(json["database"], "salt_sagatec");
        assert_eq!(json["database_charset"], "utf8mb4");
        assert_eq!(json["artifact_sha256"].as_str().unwrap().len(), 64);
        assert!(json.get("password").is_none());
    }

    #[test]
    fn rejects_impossible_completion_metadata() {
        assert_eq!(
            metadata(99, 512).unwrap_err(),
            DumpMetadataError::InvalidTimestamps
        );
        assert_eq!(
            metadata(101, 0).unwrap_err(),
            DumpMetadataError::EmptyArtifact
        );
    }

    #[test]
    fn dump_id_accepts_only_its_canonical_form() {
        let id = DumpId::new();
        assert_eq!(id.to_string().parse::<DumpId>().unwrap(), id);
        assert!(id.to_string().replace('-', "").parse::<DumpId>().is_err());
        assert!(id.to_string().to_uppercase().parse::<DumpId>().is_err());
    }
}
