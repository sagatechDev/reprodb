use std::sync::Arc;

use thiserror::Error;

use crate::{
    application::{
        DumpService, DumpServiceError, DumpStatusObserver, NoDumpStatus, PullDumpDependencies,
        RemoteProfileChoice, RemoteTargetGate, RemoteTargetGateError, RemoteTargetProbe,
        RestoreDumpChoice, RestoreService, RestoreServiceError,
    },
    domain::{DatabaseName, DumpId, MysqlVersion, ProfileName},
    infrastructure::{
        compression::{CompressionProgressObserver, NoCompressionProgress},
        config::ConfigRepository,
        credentials::CredentialStore,
        mysql::{RemoteImportError, RemoteImportExecutor},
        operation_lock::{OperationLockError, OperationLockKey, OperationLockManager},
        restore_artifact::{
            LocalRestoreArtifactValidator, RestoreArtifactError, RestoreArtifactLookup,
        },
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PushDumpChoice {
    Existing(DumpId),
    /// Create a new dump from the active profile, exactly like `pull`.
    Fresh,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PushPlan {
    pub source_profile: ProfileName,
    pub source_database: DatabaseName,
    pub dump_id: DumpId,
    pub source_version: MysqlVersion,
    pub destination_profile: ProfileName,
    pub destination_host: String,
    pub destination_port: u16,
    pub destination_user: String,
    pub database: DatabaseName,
    pub destination_version: MysqlVersion,
}

/// Every choice `push` needs from the user.
///
/// Implementations must refuse rather than guess when they cannot ask: each
/// answer decides what is written to a server other than the local target.
pub trait PushSelector: Send + Sync {
    fn profile(&self, choices: &[RemoteProfileChoice]) -> Result<ProfileName, PushSelectionError>;

    fn dump(
        &self,
        database: &DatabaseName,
        choices: &[RestoreDumpChoice],
    ) -> Result<PushDumpChoice, PushSelectionError>;

    fn database(&self, source_database: &DatabaseName) -> Result<DatabaseName, PushSelectionError>;

    fn confirm(&self, plan: &PushPlan) -> Result<bool, PushSelectionError>;
}

#[derive(Debug, Error)]
pub enum PushSelectionError {
    #[error("{0}")]
    Unavailable(String),

    #[error("dump `{0}` is not stored for this database; see `reprodb cache list`")]
    UnknownDump(DumpId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PushProgress {
    CreatingDump,
    DumpReady(DumpId),
    Importing,
}

pub trait PushProgressObserver: Send + Sync {
    fn update(&self, progress: &PushProgress);
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NoPushProgress;

impl PushProgressObserver for NoPushProgress {
    fn update(&self, _progress: &PushProgress) {}
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PushReady {
    pub plan: PushPlan,
    pub imported_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PushOutcome {
    Ready(PushReady),
    Cancelled,
}

pub struct PushService {
    repository: ConfigRepository,
    progress: Arc<dyn PushProgressObserver>,
    compression_progress: Arc<dyn CompressionProgressObserver>,
    dump_status: Arc<dyn DumpStatusObserver>,
}

impl PushService {
    pub fn new(repository: ConfigRepository) -> Self {
        Self {
            repository,
            progress: Arc::new(NoPushProgress),
            compression_progress: Arc::new(NoCompressionProgress),
            dump_status: Arc::new(NoDumpStatus),
        }
    }

    pub fn with_progress(mut self, progress: Arc<dyn PushProgressObserver>) -> Self {
        self.progress = progress;
        self
    }

    pub fn with_compression_progress(
        mut self,
        progress: Arc<dyn CompressionProgressObserver>,
    ) -> Self {
        self.compression_progress = progress;
        self
    }

    pub fn with_dump_status(mut self, status: Arc<dyn DumpStatusObserver>) -> Self {
        self.dump_status = status;
        self
    }

    pub async fn push<E>(
        &self,
        credentials: &dyn CredentialStore,
        probe: &dyn RemoteTargetProbe,
        dump: PullDumpDependencies<'_>,
        executor: E,
        selector: &dyn PushSelector,
        database: DatabaseName,
    ) -> Result<PushOutcome, PushServiceError>
    where
        E: RemoteImportExecutor,
    {
        let gate = RemoteTargetGate::new(self.repository.clone());
        let profiles = gate.eligible_profiles()?;
        if profiles.is_empty() {
            return Err(PushServiceError::NoEligibleProfile);
        }
        let profile = selector.profile(&profiles)?;
        // The destination is attested before any dump work: a wrong password
        // or an unreachable sandbox should not cost a production export.
        let guarded = gate.verify(credentials, probe, &profile).await?;

        let choices = RestoreService::new(self.repository.clone()).dump_choices(&database)?;
        let dump_id = match selector.dump(&database, &choices)? {
            PushDumpChoice::Existing(dump_id) => dump_id,
            PushDumpChoice::Fresh => {
                self.progress.update(&PushProgress::CreatingDump);
                let created = DumpService::new(self.repository.clone())
                    .with_progress(Arc::clone(&self.compression_progress))
                    .with_status(Arc::clone(&self.dump_status))
                    .create(credentials, dump.preflight, dump.executor, database.clone())
                    .await?;
                self.progress
                    .update(&PushProgress::DumpReady(created.dump_id));
                created.dump_id
            }
        };
        let artifact = LocalRestoreArtifactValidator::new(self.repository.paths().cache_dir())
            .validate_by_id(RestoreArtifactLookup {
                database: &database,
                dump_id,
            })
            .await?;
        let metadata = artifact.metadata();
        let target_database = selector.database(&metadata.database)?;
        let source_accepts_push = gate.source_accepts_push(metadata)?;
        let target = guarded.authorize(target_database, metadata, source_accepts_push)?;
        let plan = PushPlan {
            source_profile: metadata.profile.clone(),
            source_database: metadata.database.clone(),
            dump_id: metadata.dump_id,
            source_version: metadata.source_version,
            destination_profile: target.profile().clone(),
            destination_host: target.host().to_owned(),
            destination_port: target.port(),
            destination_user: target.username().to_owned(),
            database: target.database().clone(),
            destination_version: target.server_version(),
        };
        if !selector.confirm(&plan)? {
            return Ok(PushOutcome::Cancelled);
        }

        let _lock = OperationLockManager::new(self.repository.paths().cache_dir()).try_acquire(
            OperationLockKey::remote(target.profile(), target.database()),
        )?;
        self.progress.update(&PushProgress::Importing);
        executor.ensure_database(&target, metadata).await?;
        let metrics = executor.import(&target, &artifact).await?;
        if metrics.imported_bytes() != metadata.uncompressed_bytes {
            return Err(PushServiceError::ImportedSizeMismatch);
        }

        Ok(PushOutcome::Ready(PushReady {
            plan,
            imported_bytes: metrics.imported_bytes(),
        }))
    }
}

#[derive(Debug, Error)]
pub enum PushServiceError {
    #[error(
        "no profile accepts pushes; allow a non-production one with `reprodb profile allow-push NAME`"
    )]
    NoEligibleProfile,

    #[error(transparent)]
    Selection(#[from] PushSelectionError),

    #[error(transparent)]
    Target(#[from] RemoteTargetGateError),

    #[error(transparent)]
    DumpChoices(#[from] RestoreServiceError),

    #[error(transparent)]
    Dump(#[from] DumpServiceError),

    #[error(transparent)]
    Artifact(#[from] RestoreArtifactError),

    #[error(transparent)]
    Lock(#[from] OperationLockError),

    #[error(transparent)]
    Import(#[from] RemoteImportError),

    #[error(
        "mysql consumed a different number of bytes than the validated dump; the remote database may be partially imported"
    )]
    ImportedSizeMismatch,
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex},
    };

    use async_trait::async_trait;
    use secrecy::SecretString;
    use tempfile::tempdir;

    use crate::{
        application::{
            AuthorizedRemoteTarget, DumpPreflightGateway, DumpSource, RemoteTargetProbeRequest,
        },
        domain::{CredentialKey, CredentialScope, DumpArtifactMetadata, MysqlTlsMode},
        infrastructure::{
            compression::CompressionMetrics,
            config::{
                AppConfig, AppPaths, ClientRuntimeConfig, MysqlClientConfig, MysqlFamily,
                SourceProfileConfig,
            },
            credentials::MemoryCredentialStore,
            mysql::{
                ApprovedMysqlDump, ClientCatalog, DockerClientError, DumpExecutionRequest,
                DumpExecutor, DumpExecutorError, DumpPreflightError, MysqlServerInfo,
                RestoreMetrics,
            },
            restore_artifact::{ValidatedRestoreArtifact, test_support::validated_artifact},
        },
    };

    use super::*;

    struct FakeProbe;

    #[async_trait]
    impl RemoteTargetProbe for FakeProbe {
        async fn probe(
            &self,
            _request: RemoteTargetProbeRequest<'_>,
        ) -> Result<MysqlServerInfo, DockerClientError> {
            Ok(MysqlServerInfo {
                version: "8.4.4".parse().unwrap(),
                vendor: "MySQL Community Server - GPL".to_owned(),
                server_uuid: "33333333-3333-4333-8333-333333333333".parse().unwrap(),
                tls_cipher: Some("TLS_AES_256_GCM_SHA384".to_owned()),
            })
        }
    }

    /// `push` must never reach the dump path when a cached dump is chosen.
    struct UnusedDump;

    #[async_trait]
    impl DumpPreflightGateway for UnusedDump {
        async fn assess(
            &self,
            _source: &DumpSource<'_>,
            _database: &DatabaseName,
        ) -> Result<ApprovedMysqlDump, DumpPreflightError> {
            panic!("a cached push must not contact the source");
        }
    }

    #[async_trait]
    impl DumpExecutor for UnusedDump {
        async fn execute(
            &self,
            _request: DumpExecutionRequest<'_>,
            _output: std::fs::File,
        ) -> Result<CompressionMetrics, DumpExecutorError> {
            panic!("a cached push must not dump");
        }
    }

    struct FakeExecutor {
        calls: Arc<Mutex<Vec<&'static str>>>,
        imported_bytes_delta: i64,
    }

    #[async_trait]
    impl RemoteImportExecutor for FakeExecutor {
        async fn ensure_database(
            &self,
            _target: &AuthorizedRemoteTarget,
            _metadata: &DumpArtifactMetadata,
        ) -> Result<(), RemoteImportError> {
            self.calls.lock().unwrap().push("ensure");
            Ok(())
        }

        async fn import(
            &self,
            _target: &AuthorizedRemoteTarget,
            artifact: &ValidatedRestoreArtifact,
        ) -> Result<RestoreMetrics, RemoteImportError> {
            self.calls.lock().unwrap().push("import");
            let bytes = artifact.metadata().uncompressed_bytes as i64 + self.imported_bytes_delta;
            Ok(RestoreMetrics::new_for_test(bytes as u64))
        }
    }

    struct FakeSelector {
        dump: PushDumpChoice,
        database: &'static str,
        confirm: bool,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    impl PushSelector for FakeSelector {
        fn profile(
            &self,
            choices: &[RemoteProfileChoice],
        ) -> Result<ProfileName, PushSelectionError> {
            self.calls.lock().unwrap().push("profile");
            Ok(choices[0].profile.clone())
        }

        fn dump(
            &self,
            _database: &DatabaseName,
            _choices: &[RestoreDumpChoice],
        ) -> Result<PushDumpChoice, PushSelectionError> {
            self.calls.lock().unwrap().push("dump");
            Ok(self.dump)
        }

        fn database(
            &self,
            _source_database: &DatabaseName,
        ) -> Result<DatabaseName, PushSelectionError> {
            self.calls.lock().unwrap().push("database");
            Ok(DatabaseName::try_from(self.database).unwrap())
        }

        fn confirm(&self, _plan: &PushPlan) -> Result<bool, PushSelectionError> {
            self.calls.lock().unwrap().push("confirm");
            Ok(self.confirm)
        }
    }

    async fn fixture(
        root: &std::path::Path,
        with_sandbox: bool,
    ) -> (ConfigRepository, MemoryCredentialStore, DumpId) {
        let paths = AppPaths::new(root.join("config"), root.join("cache"), root.join("data"));
        let repository = ConfigRepository::new(paths.clone());
        let key = CredentialKey::new(CredentialScope::Source);
        let mut profiles = BTreeMap::new();
        if with_sandbox {
            profiles.insert(
                ProfileName::try_from("sandbox").unwrap(),
                SourceProfileConfig {
                    host: "sandbox.db.internal".to_owned(),
                    port: 3306,
                    username: "sandbox_writer".to_owned(),
                    credential_key: key,
                    mysql_family: MysqlFamily::Mysql,
                    mysql_series: "8.4".to_owned(),
                    production: false,
                    push_destination: true,
                    tls_mode: MysqlTlsMode::Required,
                    tls_material: Default::default(),
                    client: MysqlClientConfig {
                        image: ClientCatalog::resolve("8.4").unwrap().image().to_owned(),
                    },
                },
            );
        }
        repository
            .save(&AppConfig {
                client_runtime: ClientRuntimeConfig {
                    docker_context: Some("desktop-linux".to_owned()),
                    ..ClientRuntimeConfig::default()
                },
                profiles,
                ..AppConfig::default()
            })
            .unwrap();
        let credentials = MemoryCredentialStore::default();
        credentials
            .set(&key, SecretString::from("sandbox-password"))
            .await
            .unwrap();
        let artifact =
            validated_artifact(paths.cache_dir(), b"CREATE TABLE `t` (`id` INT);\n").await;
        let dump_id = artifact.metadata().dump_id;
        drop(artifact);
        (repository, credentials, dump_id)
    }

    fn acme() -> DatabaseName {
        DatabaseName::try_from("acme_production").unwrap()
    }

    #[tokio::test]
    async fn a_confirmed_push_attests_the_destination_then_creates_and_imports() {
        let directory = tempdir().unwrap();
        let (repository, credentials, dump_id) = fixture(directory.path(), true).await;
        let calls = Arc::new(Mutex::new(Vec::new()));

        let outcome = PushService::new(repository)
            .push(
                &credentials,
                &FakeProbe,
                PullDumpDependencies {
                    preflight: &UnusedDump,
                    executor: &UnusedDump,
                },
                FakeExecutor {
                    calls: Arc::clone(&calls),
                    imported_bytes_delta: 0,
                },
                &FakeSelector {
                    dump: PushDumpChoice::Existing(dump_id),
                    database: "acme_qa",
                    confirm: true,
                    calls: Arc::clone(&calls),
                },
                acme(),
            )
            .await
            .unwrap();

        assert_eq!(
            *calls.lock().unwrap(),
            ["profile", "dump", "database", "confirm", "ensure", "import"]
        );
        let PushOutcome::Ready(ready) = outcome else {
            panic!("expected a completed push");
        };
        assert_eq!(ready.plan.destination_profile.as_str(), "sandbox");
        assert_eq!(ready.plan.database.as_str(), "acme_qa");
        assert_eq!(ready.plan.source_database, acme());
        assert_eq!(ready.plan.dump_id, dump_id);
    }

    #[tokio::test]
    async fn a_declined_confirmation_never_reaches_the_executor() {
        let directory = tempdir().unwrap();
        let (repository, credentials, dump_id) = fixture(directory.path(), true).await;
        let calls = Arc::new(Mutex::new(Vec::new()));

        let outcome = PushService::new(repository)
            .push(
                &credentials,
                &FakeProbe,
                PullDumpDependencies {
                    preflight: &UnusedDump,
                    executor: &UnusedDump,
                },
                FakeExecutor {
                    calls: Arc::clone(&calls),
                    imported_bytes_delta: 0,
                },
                &FakeSelector {
                    dump: PushDumpChoice::Existing(dump_id),
                    database: "acme_qa",
                    confirm: false,
                    calls: Arc::clone(&calls),
                },
                acme(),
            )
            .await
            .unwrap();

        assert_eq!(outcome, PushOutcome::Cancelled);
        assert!(!calls.lock().unwrap().contains(&"ensure"));
        assert!(!calls.lock().unwrap().contains(&"import"));
    }

    #[tokio::test]
    async fn without_a_non_production_profile_nothing_is_asked() {
        let directory = tempdir().unwrap();
        let (repository, credentials, dump_id) = fixture(directory.path(), false).await;
        let calls = Arc::new(Mutex::new(Vec::new()));

        let error = PushService::new(repository)
            .push(
                &credentials,
                &FakeProbe,
                PullDumpDependencies {
                    preflight: &UnusedDump,
                    executor: &UnusedDump,
                },
                FakeExecutor {
                    calls: Arc::clone(&calls),
                    imported_bytes_delta: 0,
                },
                &FakeSelector {
                    dump: PushDumpChoice::Existing(dump_id),
                    database: "acme_qa",
                    confirm: true,
                    calls: Arc::clone(&calls),
                },
                acme(),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, PushServiceError::NoEligibleProfile));
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_short_import_is_reported_as_a_failure() {
        let directory = tempdir().unwrap();
        let (repository, credentials, dump_id) = fixture(directory.path(), true).await;
        let calls = Arc::new(Mutex::new(Vec::new()));

        let error = PushService::new(repository)
            .push(
                &credentials,
                &FakeProbe,
                PullDumpDependencies {
                    preflight: &UnusedDump,
                    executor: &UnusedDump,
                },
                FakeExecutor {
                    calls: Arc::clone(&calls),
                    imported_bytes_delta: -1,
                },
                &FakeSelector {
                    dump: PushDumpChoice::Existing(dump_id),
                    database: "acme_qa",
                    confirm: true,
                    calls,
                },
                acme(),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, PushServiceError::ImportedSizeMismatch));
    }
}
