use std::path::PathBuf;

use thiserror::Error;

use crate::{
    application::AuthorizedLocalTarget,
    domain::{DatabaseName, DumpId},
    infrastructure::{
        mysql::{RestoreExecutor, RestoreExecutorError},
        operation_lock::{OperationLockError, OperationLockKey, OperationLockManager},
        restore_artifact::ValidatedRestoreArtifact,
        restore_state::{LocalRestoreStateStore, RestoreStateError, RestoreStatus},
    },
};

pub struct RestoreEngine<E> {
    executor: E,
    locks: OperationLockManager,
    states: LocalRestoreStateStore,
}

impl<E> RestoreEngine<E>
where
    E: RestoreExecutor,
{
    pub fn new(executor: E, cache_root: impl Into<PathBuf>, data_root: impl Into<PathBuf>) -> Self {
        Self {
            executor,
            locks: OperationLockManager::new(cache_root),
            states: LocalRestoreStateStore::new(data_root),
        }
    }

    pub async fn restore(
        &self,
        target: &AuthorizedLocalTarget,
        artifact: &ValidatedRestoreArtifact,
    ) -> Result<RestoreCompleted, RestoreEngineError> {
        let metadata = artifact.metadata();
        if target.database() != &metadata.database {
            return Err(RestoreEngineError::DatabaseMismatch);
        }
        if mysql_series(target.server_version()) != mysql_series(metadata.source_version)
            || mysql_series(target.client().version()) != mysql_series(metadata.client_version)
        {
            return Err(RestoreEngineError::VersionMismatch);
        }

        let _lock = self.locks.try_acquire(OperationLockKey::target(
            target.docker_context(),
            target.container_id(),
            target.database(),
        ))?;

        // "Incomplete" is persisted before the first destructive statement. A crash,
        // failed CREATE or failed import therefore cannot leave stale "ready" state.
        self.states
            .save(target, metadata.dump_id, RestoreStatus::Incomplete)?;
        self.executor.recreate_database(target, metadata).await?;
        let metrics = self.executor.import(target, artifact).await?;
        if metrics.imported_bytes() != metadata.uncompressed_bytes {
            return Err(RestoreEngineError::ImportedSizeMismatch);
        }
        self.states
            .save(target, metadata.dump_id, RestoreStatus::Ready)?;

        Ok(RestoreCompleted {
            tenant_id: metadata.tenant_id.clone(),
            database: metadata.database.clone(),
            dump_id: metadata.dump_id,
            imported_bytes: metrics.imported_bytes(),
        })
    }
}

fn mysql_series(version: crate::domain::MysqlVersion) -> (u16, u16) {
    (version.major, version.minor)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RestoreCompleted {
    tenant_id: crate::domain::TenantId,
    database: DatabaseName,
    dump_id: DumpId,
    imported_bytes: u64,
}

impl RestoreCompleted {
    pub fn tenant_id(&self) -> &crate::domain::TenantId {
        &self.tenant_id
    }

    pub fn database(&self) -> &DatabaseName {
        &self.database
    }

    pub const fn dump_id(&self) -> DumpId {
        self.dump_id
    }

    pub const fn imported_bytes(&self) -> u64 {
        self.imported_bytes
    }

    #[cfg(test)]
    pub(crate) fn for_test(database: DatabaseName) -> Self {
        Self {
            tenant_id: crate::domain::TenantId::try_from("salt_sagatec").unwrap(),
            database,
            dump_id: DumpId::new(),
            imported_bytes: 100,
        }
    }
}

#[derive(Debug, Error)]
pub enum RestoreEngineError {
    #[error("the authorized target database differs from the managed dump database")]
    DatabaseMismatch,
    #[error("the target server/client series is incompatible with the managed dump")]
    VersionMismatch,
    #[error(transparent)]
    Lock(#[from] OperationLockError),
    #[error(transparent)]
    State(#[from] RestoreStateError),
    #[error(transparent)]
    Execution(#[from] RestoreExecutorError),
    #[error("mysql consumed a different number of bytes than the validated dump")]
    ImportedSizeMismatch,
}

#[cfg(test)]
mod tests {
    use std::{
        io::Cursor,
        sync::{Arc, Mutex},
    };

    use async_trait::async_trait;
    use tempfile::tempdir;

    use crate::{
        domain::{
            DatabaseEncoding, DumpArtifactCompletion, DumpArtifactContext, MysqlVersion,
            ProfileName, Sha256Digest, TenantId, TenantLookup,
        },
        infrastructure::{
            artifact_store::LocalArtifactStore,
            compression::{NoCompressionProgress, ZstdCompressor},
            mysql::RestoreMetrics,
            restore_artifact::{LocalRestoreArtifactValidator, RestoreArtifactRequest},
        },
    };

    use super::*;

    struct FakeExecutor {
        calls: Arc<Mutex<Vec<&'static str>>>,
        fail_import: bool,
    }

    #[async_trait]
    impl RestoreExecutor for FakeExecutor {
        async fn recreate_database(
            &self,
            _target: &AuthorizedLocalTarget,
            _metadata: &crate::domain::DumpArtifactMetadata,
        ) -> Result<(), RestoreExecutorError> {
            self.calls.lock().unwrap().push("recreate");
            Ok(())
        }

        async fn import(
            &self,
            _target: &AuthorizedLocalTarget,
            artifact: &ValidatedRestoreArtifact,
        ) -> Result<RestoreMetrics, RestoreExecutorError> {
            self.calls.lock().unwrap().push("import");
            if self.fail_import {
                return Err(RestoreExecutorError::MysqlStoppedEarly);
            }
            Ok(RestoreMetrics::new_for_test(
                artifact.metadata().uncompressed_bytes,
            ))
        }
    }

    async fn artifact(root: &std::path::Path) -> ValidatedRestoreArtifact {
        let profile = ProfileName::try_from("local-source").unwrap();
        let tenant = TenantId::try_from("salt_sagatec").unwrap();
        let store = LocalArtifactStore::new(root);
        let stage = store.begin(&profile, &tenant).unwrap();
        let dump_id = stage.dump_id();
        let output = stage.create_dump_writer().unwrap();
        let metrics = ZstdCompressor::default()
            .compress(
                Cursor::new(b"CREATE TABLE `items` (`id` BIGINT);\n"),
                output,
                Arc::new(NoCompressionProgress),
            )
            .await
            .unwrap();
        let metadata = crate::domain::DumpArtifactMetadata::try_new(
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
        LocalRestoreArtifactValidator::new(root)
            .validate(RestoreArtifactRequest {
                profile: &profile,
                tenant_id: &tenant,
                dump_id,
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn recreates_then_imports_and_only_then_marks_target_ready() {
        let directory = tempdir().unwrap();
        let artifact = artifact(&directory.path().join("cache")).await;
        let target =
            AuthorizedLocalTarget::for_test(DatabaseName::try_from("salt_sagatec").unwrap());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let engine = RestoreEngine::new(
            FakeExecutor {
                calls: Arc::clone(&calls),
                fail_import: false,
            },
            directory.path().join("cache"),
            directory.path().join("data"),
        );

        let completed = engine.restore(&target, &artifact).await.unwrap();

        assert_eq!(*calls.lock().unwrap(), ["recreate", "import"]);
        assert_eq!(completed.database().as_str(), "salt_sagatec");
        assert_eq!(
            engine.states.load(&target).unwrap().status,
            RestoreStatus::Ready
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            let state_directory = directory.path().join("data/restores");
            let state_file = std::fs::read_dir(&state_directory)
                .unwrap()
                .next()
                .unwrap()
                .unwrap();
            assert_eq!(
                std::fs::metadata(&state_directory)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                state_file.metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert!(!state_file.file_name().to_string_lossy().contains("sagatec"));
        }
    }

    #[tokio::test]
    async fn failed_import_remains_incomplete_and_same_artifact_can_be_retried() {
        let directory = tempdir().unwrap();
        let artifact = artifact(&directory.path().join("cache")).await;
        let target =
            AuthorizedLocalTarget::for_test(DatabaseName::try_from("salt_sagatec").unwrap());
        let engine = RestoreEngine::new(
            FakeExecutor {
                calls: Arc::new(Mutex::new(Vec::new())),
                fail_import: true,
            },
            directory.path().join("cache"),
            directory.path().join("data"),
        );

        assert!(matches!(
            engine.restore(&target, &artifact).await,
            Err(RestoreEngineError::Execution(_))
        ));
        let state = engine.states.load(&target).unwrap();
        assert_eq!(state.status, RestoreStatus::Incomplete);
        assert_eq!(state.dump_id, artifact.metadata().dump_id);

        let retry = RestoreEngine::new(
            FakeExecutor {
                calls: Arc::new(Mutex::new(Vec::new())),
                fail_import: false,
            },
            directory.path().join("cache"),
            directory.path().join("data"),
        );
        retry.restore(&target, &artifact).await.unwrap();
        assert_eq!(
            retry.states.load(&target).unwrap().status,
            RestoreStatus::Ready
        );
    }

    #[tokio::test]
    async fn database_mismatch_is_rejected_before_executor_or_state() {
        let directory = tempdir().unwrap();
        let artifact = artifact(&directory.path().join("cache")).await;
        let target =
            AuthorizedLocalTarget::for_test(DatabaseName::try_from("salt_polymer").unwrap());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let engine = RestoreEngine::new(
            FakeExecutor {
                calls: Arc::clone(&calls),
                fail_import: false,
            },
            directory.path().join("cache"),
            directory.path().join("data"),
        );

        assert!(matches!(
            engine.restore(&target, &artifact).await,
            Err(RestoreEngineError::DatabaseMismatch)
        ));
        assert!(calls.lock().unwrap().is_empty());
        assert!(!directory.path().join("data").exists());
    }
}
