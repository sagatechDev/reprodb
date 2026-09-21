use std::sync::Arc;

use thiserror::Error;

use crate::{
    application::{
        LocalTargetAttestor, LocalTargetGate, LocalTargetGateError, RestoreEngine,
        RestoreEngineError,
    },
    domain::{ContainerName, DatabaseName, DumpId, MysqlVersion, ProfileName},
    infrastructure::{
        artifact_store::LocalArtifactStore,
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
    /// `None` asks the selector which of the stored dumps to restore.
    pub dump_id: Option<DumpId>,
    pub target_container: Option<ContainerName>,
    pub target_database: Option<DatabaseName>,
}

/// One dump offered to the user when `--dump-id` was omitted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RestoreDumpChoice {
    pub profile: ProfileName,
    pub dump_id: DumpId,
    pub completed_at_unix_seconds: u64,
    pub compressed_bytes: u64,
    pub source_version: MysqlVersion,
}

/// Chooses among the dumps stored for a database.
///
/// Implementations must not choose on the user's behalf when they cannot ask:
/// restore recreates the local database, so an unattended guess is destructive.
pub trait RestoreDumpSelector: Send + Sync {
    fn select(
        &self,
        database: &DatabaseName,
        choices: &[RestoreDumpChoice],
    ) -> Result<DumpId, RestoreSelectionError>;
}

/// Rejects any request that would need a choice made for it.
///
/// Used by callers that always name the dump themselves, such as `pull`.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoRestoreDumpSelector;

impl RestoreDumpSelector for NoRestoreDumpSelector {
    fn select(
        &self,
        _database: &DatabaseName,
        _choices: &[RestoreDumpChoice],
    ) -> Result<DumpId, RestoreSelectionError> {
        Err(RestoreSelectionError::Unavailable(
            "this command requires an explicit dump ID".to_owned(),
        ))
    }
}

#[derive(Debug, Error)]
pub enum RestoreSelectionError {
    #[error("no managed dump found for database `{database}`")]
    NoCandidates { database: DatabaseName },

    #[error("{0}")]
    Unavailable(String),
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
            &NoRestoreDumpSelector,
            RestoreRequest {
                database,
                dump_id: Some(dump_id),
                target_container: None,
                target_database: None,
            },
        )
        .await
    }

    /// Lists the stored dumps for `database`, newest first.
    ///
    /// Reads metadata only. The expensive validation (size, Zstd and both
    /// SHA-256 digests) runs once, on the dump the user picks, so offering six
    /// candidates does not mean hashing six artifacts.
    fn dump_choices(
        &self,
        database: &DatabaseName,
    ) -> Result<Vec<RestoreDumpChoice>, RestoreServiceError> {
        let store = LocalArtifactStore::new(self.repository.paths().cache_dir());
        let mut choices = Vec::new();
        for located in store.find_complete_by_database(database)? {
            let (profile, _, artifact) = located.into_parts();
            let Ok(contents) = std::fs::read_to_string(&artifact.metadata_path) else {
                continue;
            };
            let Ok(metadata) =
                serde_json::from_str::<crate::domain::DumpArtifactMetadata>(&contents)
            else {
                continue;
            };
            if metadata.dump_id != artifact.dump_id || metadata.database != *database {
                continue;
            }
            choices.push(RestoreDumpChoice {
                profile,
                dump_id: artifact.dump_id,
                completed_at_unix_seconds: metadata.completed_at_unix_seconds,
                compressed_bytes: metadata.compressed_bytes,
                source_version: metadata.source_version,
            });
        }
        choices.sort_by(|left, right| {
            right
                .completed_at_unix_seconds
                .cmp(&left.completed_at_unix_seconds)
                .then_with(|| right.dump_id.cmp(&left.dump_id))
        });
        Ok(choices)
    }

    pub async fn restore_to<E>(
        &self,
        credentials: &dyn CredentialStore,
        attestor: &dyn LocalTargetAttestor,
        executor: E,
        selector: &dyn RestoreDumpSelector,
        request: RestoreRequest,
    ) -> Result<RestoreReady, RestoreServiceError>
    where
        E: RestoreExecutor,
    {
        let dump_id = match request.dump_id {
            Some(dump_id) => dump_id,
            None => selector.select(&request.database, &self.dump_choices(&request.database)?)?,
        };
        let artifact = LocalRestoreArtifactValidator::new(self.repository.paths().cache_dir())
            .validate_by_id(RestoreArtifactLookup {
                database: &request.database,
                dump_id,
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

    #[error(transparent)]
    Selection(#[from] RestoreSelectionError),

    #[error(transparent)]
    Store(#[from] crate::infrastructure::artifact_store::ArtifactStoreError),
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

    /// Publishes an extra dump of the same database, optionally under another
    /// profile, so the selector has more than one candidate to offer.
    async fn publish_extra(
        repository: &ConfigRepository,
        profile_name: &str,
        completed_at_unix_seconds: u64,
    ) -> DumpId {
        let profile = ProfileName::try_from(profile_name).unwrap();
        let database = DatabaseName::try_from("acme_production").unwrap();
        let store = LocalArtifactStore::new(repository.paths().cache_dir());
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
                database,
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
                created_at_unix_seconds: completed_at_unix_seconds - 1,
                completed_at_unix_seconds,
                uncompressed_bytes: metrics.input_bytes(),
                compressed_bytes: metrics.compressed_bytes(),
                sql_sha256: metrics.input_sha256(),
                artifact_sha256: metrics.compressed_sha256(),
            },
        )
        .unwrap();
        stage.publish(&metadata, &metrics).unwrap();
        dump_id
    }

    #[tokio::test]
    async fn the_selector_is_offered_every_profile_newest_first() {
        let directory = tempdir().unwrap();
        let (repository, _credentials, first) = configured_fixture(directory.path()).await;
        let newest = publish_extra(&repository, "local-source", 900).await;
        let staging = publish_extra(&repository, "staging", 500).await;

        let choices = RestoreService::new(repository)
            .dump_choices(&DatabaseName::try_from("acme_production").unwrap())
            .unwrap();

        assert_eq!(
            choices
                .iter()
                .map(|choice| choice.dump_id)
                .collect::<Vec<_>>(),
            vec![newest, staging, first],
        );
        // Restore does not depend on the active profile, so neither does the list.
        assert!(
            choices
                .iter()
                .any(|choice| choice.profile.as_str() == "staging")
        );
    }

    #[tokio::test]
    async fn dumps_of_another_database_are_never_offered() {
        let directory = tempdir().unwrap();
        let (repository, _credentials, _dump_id) = configured_fixture(directory.path()).await;

        let choices = RestoreService::new(repository)
            .dump_choices(&DatabaseName::try_from("globex_production").unwrap())
            .unwrap();

        assert!(choices.is_empty());
    }

    #[tokio::test]
    async fn an_omitted_dump_id_asks_the_selector_and_restores_its_answer() {
        let directory = tempdir().unwrap();
        let (repository, credentials, first) = configured_fixture(directory.path()).await;
        let newest = publish_extra(&repository, "local-source", 900).await;

        struct PickOldest;
        impl RestoreDumpSelector for PickOldest {
            fn select(
                &self,
                _database: &DatabaseName,
                choices: &[RestoreDumpChoice],
            ) -> Result<DumpId, RestoreSelectionError> {
                Ok(choices.last().unwrap().dump_id)
            }
        }

        let restored = RestoreService::new(repository)
            .restore_to(
                &credentials,
                &FakeAttestor {
                    calls: Arc::new(Mutex::new(0)),
                },
                FakeExecutor {
                    calls: Arc::new(Mutex::new(Vec::new())),
                },
                &PickOldest,
                RestoreRequest {
                    database: DatabaseName::try_from("acme_production").unwrap(),
                    dump_id: None,
                    target_container: None,
                    target_database: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(restored.plan.dump_id, first);
        assert_ne!(restored.plan.dump_id, newest);
    }

    #[tokio::test]
    async fn a_selector_that_cannot_ask_stops_the_restore_before_any_local_change() {
        let directory = tempdir().unwrap();
        let (repository, credentials, _dump_id) = configured_fixture(directory.path()).await;

        struct CannotAsk;
        impl RestoreDumpSelector for CannotAsk {
            fn select(
                &self,
                _database: &DatabaseName,
                _choices: &[RestoreDumpChoice],
            ) -> Result<DumpId, RestoreSelectionError> {
                Err(RestoreSelectionError::Unavailable("no terminal".to_owned()))
            }
        }

        let recorded = Arc::new(Mutex::new(Vec::new()));
        let error = RestoreService::new(repository)
            .restore_to(
                &credentials,
                &FakeAttestor {
                    calls: Arc::new(Mutex::new(0)),
                },
                FakeExecutor {
                    calls: Arc::clone(&recorded),
                },
                &CannotAsk,
                RestoreRequest {
                    database: DatabaseName::try_from("acme_production").unwrap(),
                    dump_id: None,
                    target_container: None,
                    target_database: None,
                },
            )
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            RestoreServiceError::Selection(RestoreSelectionError::Unavailable(_))
        ));
        assert!(recorded.lock().unwrap().is_empty());
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
                &NoRestoreDumpSelector,
                RestoreRequest {
                    database: DatabaseName::try_from("acme_production").unwrap(),
                    dump_id: Some(dump_id),
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
