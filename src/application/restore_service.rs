use std::sync::Arc;

use thiserror::Error;

use crate::{
    application::{
        LocalTargetAttestor, LocalTargetGate, LocalTargetGateError, RestoreEngine,
        RestoreEngineError,
    },
    domain::{ContainerName, DatabaseName, DumpId, MysqlVersion, ProfileName},
    infrastructure::{
        config::ConfigRepository,
        credentials::CredentialStore,
        mysql::RestoreExecutor,
        restore_artifact::{
            LocalRestoreArtifactValidator, RestoreArtifactError, RestoreArtifactLookup,
        },
    },
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RestorePlan {
    pub profile: ProfileName,
    pub source_database: DatabaseName,
    pub database: DatabaseName,
    pub dump_id: DumpId,
    pub source_version: MysqlVersion,
    pub client_version: MysqlVersion,
    pub container: crate::domain::ContainerName,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RestoreProgress {
    PlanReady(RestorePlan),
    RestoringDatabase,
}

pub trait RestoreProgressObserver: Send + Sync {
    fn update(&self, progress: &RestoreProgress);
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NoRestoreProgress;

impl RestoreProgressObserver for NoRestoreProgress {
    fn update(&self, _progress: &RestoreProgress) {}
}

pub struct RestoreService {
    repository: ConfigRepository,
    progress: Arc<dyn RestoreProgressObserver>,
}

pub struct RestoreRequest {
    pub database: DatabaseName,
    pub dump_id: DumpId,
    pub target_container: Option<ContainerName>,
    pub target_database: Option<DatabaseName>,
}

impl RestoreService {
    pub fn new(repository: ConfigRepository) -> Self {
        Self {
            repository,
            progress: Arc::new(NoRestoreProgress),
        }
    }

    pub fn with_progress(mut self, progress: Arc<dyn RestoreProgressObserver>) -> Self {
        self.progress = progress;
        self
    }

    pub async fn restore<E>(
        &self,
        credentials: &dyn CredentialStore,
        attestor: &dyn LocalTargetAttestor,
        executor: E,
        database: DatabaseName,
        dump_id: DumpId,
    ) -> Result<RestoreReady, RestoreServiceError>
    where
        E: RestoreExecutor,
    {
        self.restore_to(
            credentials,
            attestor,
            executor,
            RestoreRequest {
                database,
                dump_id,
                target_container: None,
                target_database: None,
            },
        )
        .await
    }

    pub async fn restore_to<E>(
        &self,
        credentials: &dyn CredentialStore,
        attestor: &dyn LocalTargetAttestor,
        executor: E,
        request: RestoreRequest,
    ) -> Result<RestoreReady, RestoreServiceError>
    where
        E: RestoreExecutor,
    {
        let artifact = LocalRestoreArtifactValidator::new(self.repository.paths().cache_dir())
            .validate_by_id(RestoreArtifactLookup {
                database: &request.database,
                dump_id: request.dump_id,
            })
            .await?;
        let source_database = artifact.metadata().database.clone();
        let target_database = request
            .target_database
            .unwrap_or_else(|| source_database.clone());
        let guarded = LocalTargetGate::new(self.repository.clone())
            .verify_named(credentials, attestor, request.target_container.as_ref())
            .await?;
        let target = guarded.authorize_database(target_database.clone())?;
        let plan = RestorePlan {
            profile: artifact.metadata().profile.clone(),
            source_database,
            database: target_database,
            dump_id: artifact.metadata().dump_id,
            source_version: artifact.metadata().source_version,
            client_version: artifact.metadata().client_version,
            container: target.container_name().clone(),
        };
        self.progress
            .update(&RestoreProgress::PlanReady(plan.clone()));
        self.progress.update(&RestoreProgress::RestoringDatabase);

        let completed = RestoreEngine::new(
            executor,
            self.repository.paths().cache_dir(),
            self.repository.paths().data_dir(),
        )
        .restore(&target, &artifact)
        .await?;

        Ok(RestoreReady {
            plan,
            imported_bytes: completed.imported_bytes(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RestoreReady {
    pub plan: RestorePlan,
    pub imported_bytes: u64,
}

#[derive(Debug, Error)]
pub enum RestoreServiceError {
    #[error(transparent)]
    Artifact(#[from] RestoreArtifactError),

    #[error(transparent)]
    Target(#[from] LocalTargetGateError),

    #[error(transparent)]
    Engine(#[from] RestoreEngineError),
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        io::Cursor,
        path::Path,
        sync::{Arc, Mutex},
    };

    use async_trait::async_trait;
    use secrecy::SecretString;
    use tempfile::tempdir;

    use crate::{
        application::{
            AuthorizedLocalTarget, LocalTargetAttestation, LocalTargetAttestationError,
            LocalTargetAttestationRequest,
        },
        domain::{
            ContainerId, ContainerName, CredentialKey, CredentialScope, DatabaseEncoding,
            DumpArtifactCompletion, DumpArtifactContext, MysqlVersion, Sha256Digest,
        },
        infrastructure::{
            artifact_store::LocalArtifactStore,
            compression::{NoCompressionProgress, ZstdCompressor},
            config::{
                AppConfig, AppPaths, ClientRuntimeConfig, LocalTargetConfig, LocalTargetTrust,
            },
            credentials::{CredentialStore, MemoryCredentialStore},
            mysql::{ClientCatalog, RestoreExecutorError, RestoreMetrics},
            restore_artifact::ValidatedRestoreArtifact,
        },
    };

    use super::*;

    struct FakeAttestor {
        calls: Arc<Mutex<usize>>,
    }

    #[async_trait]
    impl LocalTargetAttestor for FakeAttestor {
        async fn attest(
            &self,
            _request: LocalTargetAttestationRequest<'_>,
        ) -> Result<LocalTargetAttestation, LocalTargetAttestationError> {
            *self.calls.lock().unwrap() += 1;
            Ok(LocalTargetAttestation {
                docker_context: "desktop-linux".to_owned(),
                container_name: ContainerName::try_from("mysql-8").unwrap(),
                container_id: ContainerId::try_from("a".repeat(64)).unwrap(),
                managed_by_reprodb: false,
                server_version: "8.4.4".parse().unwrap(),
                server_uuid: "22222222-2222-4222-8222-222222222222".parse().unwrap(),
                vendor: "MySQL Community Server - GPL".to_owned(),
                client: ClientCatalog::resolve("8.4").unwrap(),
            })
        }
    }

    struct FakeExecutor {
        calls: Arc<Mutex<Vec<&'static str>>>,
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
            Ok(RestoreMetrics::new_for_test(
                artifact.metadata().uncompressed_bytes,
            ))
        }
    }

    struct RecordingProgress(Arc<Mutex<Vec<RestoreProgress>>>);

    impl RestoreProgressObserver for RecordingProgress {
        fn update(&self, progress: &RestoreProgress) {
            self.0.lock().unwrap().push(progress.clone());
        }
    }

    async fn configured_fixture(root: &Path) -> (ConfigRepository, MemoryCredentialStore, DumpId) {
        let paths = AppPaths::new(root.join("config"), root.join("cache"), root.join("data"));
        let repository = ConfigRepository::new(paths.clone());
        let credential_key = CredentialKey::new(CredentialScope::Target);
        repository
            .save(&AppConfig {
                client_runtime: ClientRuntimeConfig {
                    docker_context: Some("desktop-linux".to_owned()),
                    ..ClientRuntimeConfig::default()
                },
                local_target: Some(LocalTargetConfig {
                    docker_context: "desktop-linux".to_owned(),
                    container_name: ContainerName::try_from("mysql-8").unwrap(),
                    container_id: ContainerId::try_from("a".repeat(64)).unwrap(),
                    username: "root".to_owned(),
                    credential_key,
                    trust: LocalTargetTrust::UserConfirmed,
                }),
                profiles: BTreeMap::new(),
                ..AppConfig::default()
            })
            .unwrap();
        let credentials = MemoryCredentialStore::default();
        credentials
            .set(&credential_key, SecretString::from("local-password"))
            .await
            .unwrap();

        let profile = ProfileName::try_from("local-source").unwrap();
        let database = DatabaseName::try_from("acme_production").unwrap();
        let store = LocalArtifactStore::new(paths.cache_dir());
        let stage = store.begin(&profile, &database).unwrap();
        let dump_id = stage.dump_id();
        let metrics = ZstdCompressor::default()
            .compress(
                Cursor::new(b"CREATE TABLE `items` (`id` BIGINT);\n"),
                stage.create_dump_writer().unwrap(),
                Arc::new(NoCompressionProgress),
            )
            .await
            .unwrap();
        let metadata = crate::domain::DumpArtifactMetadata::try_new(
            dump_id,
            DumpArtifactContext {
                database: DatabaseName::try_from("acme_production").unwrap(),
                profile,
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
                uncompressed_bytes: metrics.input_bytes(),
                compressed_bytes: metrics.compressed_bytes(),
                sql_sha256: metrics.input_sha256(),
                artifact_sha256: metrics.compressed_sha256(),
            },
        )
        .unwrap();
        stage.publish(&metadata, &metrics).unwrap();
        (repository, credentials, dump_id)
    }

    #[tokio::test]
    async fn validates_restores_and_registers_in_the_visible_order() {
        let directory = tempdir().unwrap();
        let (repository, credentials, dump_id) = configured_fixture(directory.path()).await;
        let attestor_calls = Arc::new(Mutex::new(0));
        let executor_calls = Arc::new(Mutex::new(Vec::new()));
        let progress = Arc::new(Mutex::new(Vec::new()));
        let service = RestoreService::new(repository)
            .with_progress(Arc::new(RecordingProgress(Arc::clone(&progress))));

        let ready = service
            .restore(
                &credentials,
                &FakeAttestor {
                    calls: Arc::clone(&attestor_calls),
                },
                FakeExecutor {
                    calls: Arc::clone(&executor_calls),
                },
                DatabaseName::try_from("acme_production").unwrap(),
                dump_id,
            )
            .await
            .unwrap();

        assert_eq!(ready.plan.database.as_str(), "acme_production");
        assert_eq!(ready.plan.source_database.as_str(), "acme_production");
        assert_eq!(*attestor_calls.lock().unwrap(), 1);
        assert_eq!(*executor_calls.lock().unwrap(), ["recreate", "import"]);
        assert!(matches!(
            progress.lock().unwrap().as_slice(),
            [
                RestoreProgress::PlanReady(_),
                RestoreProgress::RestoringDatabase
            ]
        ));
    }

    #[tokio::test]
    async fn custom_target_database_is_authorized_and_restored_without_a_central_write() {
        let directory = tempdir().unwrap();
        let (repository, credentials, dump_id) = configured_fixture(directory.path()).await;

        let ready = RestoreService::new(repository)
            .restore_to(
                &credentials,
                &FakeAttestor {
                    calls: Arc::new(Mutex::new(0)),
                },
                FakeExecutor {
                    calls: Arc::new(Mutex::new(Vec::new())),
                },
                RestoreRequest {
                    database: DatabaseName::try_from("acme_production").unwrap(),
                    dump_id,
                    target_container: None,
                    target_database: Some(DatabaseName::try_from("acme_production_debug").unwrap()),
                },
            )
            .await
            .unwrap();

        assert_eq!(ready.plan.source_database.as_str(), "acme_production");
        assert_eq!(ready.plan.database.as_str(), "acme_production_debug");
    }

    #[tokio::test]
    async fn database_mismatch_is_rejected_before_credentials_or_target_attestation() {
        let directory = tempdir().unwrap();
        let (repository, _, dump_id) = configured_fixture(directory.path()).await;
        let attestor_calls = Arc::new(Mutex::new(0));

        let error = RestoreService::new(repository)
            .restore(
                &MemoryCredentialStore::default(),
                &FakeAttestor {
                    calls: Arc::clone(&attestor_calls),
                },
                FakeExecutor {
                    calls: Arc::new(Mutex::new(Vec::new())),
                },
                DatabaseName::try_from("globex").unwrap(),
                dump_id,
            )
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            RestoreServiceError::Artifact(RestoreArtifactError::DatabaseMismatch)
        ));
        assert_eq!(*attestor_calls.lock().unwrap(), 0);
    }
}
