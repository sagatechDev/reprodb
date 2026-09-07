use std::{sync::Arc, time::Duration};

use thiserror::Error;

use crate::{
    application::{
        Clock, ClockError, DumpPreflightGateway, DumpService, DumpServiceError, DumpTenantResolver,
        LocalTargetAttestor, LocalTenantWriter, RestoreProgressObserver, RestoreReady,
        RestoreRequest, RestoreService, RestoreServiceError, SystemClock,
    },
    domain::{ContainerName, DumpId, MYSQL_8_DUMP_POLICY_VERSION, TenantLookup},
    infrastructure::{
        artifact_store::LocalArtifactStore,
        cache::{
            CacheError, CacheLookupResult, CacheMissReason, CacheTenantLookup,
            DEFAULT_CACHE_TTL_SECONDS, LocalCacheValidator,
        },
        compression::{CompressionProgressObserver, NoCompressionProgress},
        config::{ConfigError, ConfigRepository, source_profile_fingerprint},
        credentials::CredentialStore,
        mysql::{DumpExecutor, RestoreExecutor},
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PullProgress {
    CheckingCache,
    CacheHit { dump_id: DumpId, age_seconds: u64 },
    CacheMiss(CacheMissReason),
    CreatingDump,
    DumpReady(DumpId),
}

pub trait PullProgressObserver: Send + Sync {
    fn update(&self, progress: PullProgress);
}

pub trait PullDatabaseSelector: Send + Sync {
    fn select(
        &self,
        source_database: &crate::domain::DatabaseName,
    ) -> Result<crate::domain::DatabaseName, PullDatabaseSelectionError>;
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("could not select the local restore database")]
pub struct PullDatabaseSelectionError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullTargetChoice {
    pub container: ContainerName,
    pub is_default: bool,
}

pub trait PullTargetSelector: Send + Sync {
    fn select(
        &self,
        choices: &[PullTargetChoice],
    ) -> Result<ContainerName, PullTargetSelectionError>;
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("no matching local restore target is configured; run `reprodb setup`")]
pub struct PullTargetSelectionError;

#[derive(Clone, Copy, Debug, Default)]
pub struct NoPullProgress;

impl PullProgressObserver for NoPullProgress {
    fn update(&self, _progress: PullProgress) {}
}

pub struct PullService<C = SystemClock> {
    repository: ConfigRepository,
    clock: C,
    progress: Arc<dyn PullProgressObserver>,
    compression_progress: Arc<dyn CompressionProgressObserver>,
    restore_progress: Arc<dyn RestoreProgressObserver>,
}

pub struct PullDumpDependencies<'a> {
    pub tenant_resolver: &'a dyn DumpTenantResolver,
    pub preflight: &'a dyn DumpPreflightGateway,
    pub executor: &'a dyn DumpExecutor,
}

pub struct PullRestoreDependencies<'a, RE, W> {
    pub target_attestor: &'a dyn LocalTargetAttestor,
    pub executor: RE,
    pub tenant_writer: W,
    pub target_selector: &'a dyn PullTargetSelector,
    pub database_selector: &'a dyn PullDatabaseSelector,
}

impl PullService<SystemClock> {
    pub fn new(repository: ConfigRepository) -> Self {
        Self {
            repository,
            clock: SystemClock,
            progress: Arc::new(NoPullProgress),
            compression_progress: Arc::new(NoCompressionProgress),
            restore_progress: Arc::new(crate::application::NoRestoreProgress),
        }
    }
}

impl<C> PullService<C>
where
    C: Clock + Clone,
{
    #[cfg(test)]
    pub(crate) fn with_clock(repository: ConfigRepository, clock: C) -> Self {
        Self {
            repository,
            clock,
            progress: Arc::new(NoPullProgress),
            compression_progress: Arc::new(NoCompressionProgress),
            restore_progress: Arc::new(crate::application::NoRestoreProgress),
        }
    }

    pub fn with_progress(mut self, progress: Arc<dyn PullProgressObserver>) -> Self {
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

    pub fn with_restore_progress(mut self, progress: Arc<dyn RestoreProgressObserver>) -> Self {
        self.restore_progress = progress;
        self
    }

    pub async fn pull<RE, W>(
        &self,
        credentials: &dyn CredentialStore,
        dump: PullDumpDependencies<'_>,
        restore: PullRestoreDependencies<'_, RE, W>,
        tenant: TenantLookup,
        fresh: bool,
    ) -> Result<PullReady, PullServiceError>
    where
        RE: RestoreExecutor,
        W: LocalTenantWriter,
    {
        let total_started = std::time::Instant::now();
        let config = self.repository.load()?;
        let profile_name = config
            .active_profile
            .as_ref()
            .ok_or(PullServiceError::NoActiveProfile)?;
        let profile = config
            .profiles
            .get(profile_name)
            .ok_or(PullServiceError::NoActiveProfile)?;
        let target_choices = config
            .configured_local_targets()
            .map(|(target, is_default)| PullTargetChoice {
                container: target.container_name.clone(),
                is_default,
            })
            .collect::<Vec<_>>();
        let target_container = restore.target_selector.select(&target_choices)?;
        let now = self.clock.now_unix_seconds()?;

        self.progress.update(PullProgress::CheckingCache);
        let cache =
            LocalCacheValidator::new(LocalArtifactStore::new(self.repository.paths().cache_dir()))
                .lookup_by_tenant(&CacheTenantLookup {
                    profile: profile_name,
                    tenant: &tenant,
                    source_fingerprint: source_profile_fingerprint(profile_name, profile),
                    policy_version: MYSQL_8_DUMP_POLICY_VERSION,
                    now_unix_seconds: now,
                    ttl_seconds: DEFAULT_CACHE_TTL_SECONDS,
                    fresh,
                })?;

        let (dump_id, source_database, cache_use, dump_metrics, _cache_lease) = match cache {
            CacheLookupResult::Hit(hit) => {
                let dump_id = hit.metadata().dump_id;
                let age_seconds = hit.age_seconds();
                self.progress.update(PullProgress::CacheHit {
                    dump_id,
                    age_seconds,
                });
                (
                    dump_id,
                    hit.metadata().database.clone(),
                    PullCacheUse::Hit { age_seconds },
                    None,
                    Some(hit),
                )
            }
            CacheLookupResult::Miss(reason) => {
                self.progress.update(PullProgress::CacheMiss(reason));
                self.progress.update(PullProgress::CreatingDump);
                let created = DumpService::with_clock(self.repository.clone(), self.clock.clone())
                    .with_progress(Arc::clone(&self.compression_progress))
                    .create(
                        credentials,
                        dump.tenant_resolver,
                        dump.preflight,
                        dump.executor,
                        tenant.clone(),
                    )
                    .await?;
                self.progress
                    .update(PullProgress::DumpReady(created.dump_id));
                let dump_metrics = PullDumpMetrics {
                    elapsed: created.elapsed,
                    uncompressed_bytes: created.uncompressed_bytes,
                    compressed_bytes: created.compressed_bytes,
                };
                (
                    created.dump_id,
                    created.database,
                    PullCacheUse::Created,
                    Some(dump_metrics),
                    None,
                )
            }
        };
        let target_database = restore.database_selector.select(&source_database)?;

        let restore_started = std::time::Instant::now();
        let restored = RestoreService::new(self.repository.clone())
            .with_progress(Arc::clone(&self.restore_progress))
            .restore_to(
                credentials,
                restore.target_attestor,
                restore.executor,
                restore.tenant_writer,
                RestoreRequest {
                    tenant,
                    dump_id,
                    target_container: Some(target_container),
                    target_database: Some(target_database),
                },
            )
            .await?;
        let restore_elapsed = restore_started.elapsed();

        Ok(PullReady {
            cache: cache_use,
            restored,
            metrics: PullMetrics {
                dump: dump_metrics,
                restore_elapsed,
                total_elapsed: total_started.elapsed(),
            },
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PullCacheUse {
    Hit { age_seconds: u64 },
    Created,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullReady {
    pub cache: PullCacheUse,
    pub restored: RestoreReady,
    pub metrics: PullMetrics,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PullMetrics {
    pub dump: Option<PullDumpMetrics>,
    pub restore_elapsed: Duration,
    pub total_elapsed: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PullDumpMetrics {
    pub elapsed: Duration,
    pub uncompressed_bytes: u64,
    pub compressed_bytes: u64,
}

#[derive(Debug, Error)]
pub enum PullServiceError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error(
        "no active source profile; run `reprodb profile add NAME` or `reprodb profile use NAME`"
    )]
    NoActiveProfile,

    #[error(transparent)]
    Clock(#[from] ClockError),

    #[error(transparent)]
    Cache(#[from] CacheError),

    #[error(transparent)]
    Dump(#[from] DumpServiceError),

    #[error(transparent)]
    Restore(#[from] RestoreServiceError),

    #[error(transparent)]
    DatabaseSelection(#[from] PullDatabaseSelectionError),

    #[error(transparent)]
    TargetSelection(#[from] PullTargetSelectionError),
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, VecDeque},
        io::Cursor,
        path::Path,
        sync::{Arc, Mutex, atomic::AtomicUsize},
    };

    use async_trait::async_trait;
    use secrecy::SecretString;
    use tempfile::tempdir;

    use crate::{
        application::{
            AuthorizedLocalTarget, DumpSource, LocalTargetAttestation, LocalTargetAttestationError,
            LocalTargetAttestationRequest, LocalTenantWriteError,
        },
        domain::{
            ContainerId, ContainerName, CredentialKey, CredentialScope, DatabaseEncoding,
            DatabaseName, DatabaseObjectCounts, DefinerObjectCounts, DumpArtifactCompletion,
            DumpArtifactContext, DumpPreflight, GtidMode, LocalTenantFeatures, Mysql8DumpPolicy,
            MysqlTlsMode, MysqlVersion, ProfileName, ResolvedTenant, StorageEngineUsage, TenantId,
            TenantMatch, TenantResolutionError,
        },
        infrastructure::{
            artifact_store::LocalArtifactStore,
            compression::{NoCompressionProgress, ZstdCompressor},
            config::{
                AppConfig, AppPaths, ClientRuntimeConfig, LocalTargetConfig, LocalTargetTrust,
                MysqlClientConfig, MysqlFamily, SourceProfileConfig, TenantResolverConfig,
            },
            credentials::{CredentialStore, MemoryCredentialStore},
            mysql::{
                ApprovedMysqlDump, ClientCatalog, DumpExecutionRequest, DumpExecutorError,
                DumpPreflightError, MysqlServerInfo, RestoreExecutorError, RestoreMetrics,
            },
            restore_artifact::ValidatedRestoreArtifact,
        },
    };

    use super::*;

    #[derive(Clone)]
    struct TestClock(Arc<Mutex<VecDeque<u64>>>);

    impl Clock for TestClock {
        fn now_unix_seconds(&self) -> Result<u64, ClockError> {
            self.0.lock().unwrap().pop_front().ok_or(ClockError)
        }
    }

    struct UnusedDump {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl DumpTenantResolver for UnusedDump {
        async fn resolve(
            &self,
            _source: &DumpSource<'_>,
            _lookup: &TenantLookup,
        ) -> Result<ResolvedTenant, TenantResolutionError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            panic!("a cache hit must not resolve the tenant on the source")
        }
    }

    #[async_trait]
    impl DumpPreflightGateway for UnusedDump {
        async fn assess(
            &self,
            _source: &DumpSource<'_>,
            _database: &DatabaseName,
        ) -> Result<ApprovedMysqlDump, DumpPreflightError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            panic!("a cache hit must not preflight the source")
        }
    }

    #[async_trait]
    impl DumpExecutor for UnusedDump {
        async fn execute(
            &self,
            _request: DumpExecutionRequest<'_>,
            _output: std::fs::File,
        ) -> Result<crate::infrastructure::compression::CompressionMetrics, DumpExecutorError>
        {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            panic!("a cache hit must not execute mysqldump")
        }
    }

    struct SuccessfulDump {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl DumpTenantResolver for SuccessfulDump {
        async fn resolve(
            &self,
            _source: &DumpSource<'_>,
            lookup: &TenantLookup,
        ) -> Result<ResolvedTenant, TenantResolutionError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(lookup.as_str(), "sagatec");
            Ok(ResolvedTenant {
                tenant_id: TenantId::try_from("salt_sagatec").unwrap(),
                database: DatabaseName::try_from("salt_sagatec").unwrap(),
                matched_by: TenantMatch::Domain,
                features: LocalTenantFeatures::default(),
            })
        }
    }

    #[async_trait]
    impl DumpPreflightGateway for SuccessfulDump {
        async fn assess(
            &self,
            _source: &DumpSource<'_>,
            database: &DatabaseName,
        ) -> Result<ApprovedMysqlDump, DumpPreflightError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let version = "8.4.4".parse::<MysqlVersion>().unwrap();
            let preflight = DumpPreflight {
                encoding: DatabaseEncoding::try_new(
                    "utf8mb4".to_owned(),
                    "utf8mb4_0900_ai_ci".to_owned(),
                )
                .unwrap(),
                estimated_data_bytes: 1024,
                engines: vec![StorageEngineUsage::try_new("InnoDB".to_owned(), 1).unwrap()],
                objects: DatabaseObjectCounts::default(),
                definers: DefinerObjectCounts::default(),
                gtid_mode: GtidMode::On,
            };
            let plan = Mysql8DumpPolicy::evaluate(
                version,
                "MySQL Community Server - GPL",
                version,
                database,
                &preflight,
            )
            .unwrap();
            Ok(ApprovedMysqlDump {
                server: MysqlServerInfo {
                    version,
                    vendor: "MySQL Community Server - GPL".to_owned(),
                    server_uuid: "11111111-1111-4111-8111-111111111111".parse().unwrap(),
                    tls_cipher: None,
                },
                preflight,
                plan,
            })
        }
    }

    #[async_trait]
    impl DumpExecutor for SuccessfulDump {
        async fn execute(
            &self,
            request: DumpExecutionRequest<'_>,
            output: std::fs::File,
        ) -> Result<crate::infrastructure::compression::CompressionMetrics, DumpExecutorError>
        {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ZstdCompressor::default()
                .compress(
                    Cursor::new(b"CREATE TABLE `fresh_items` (`id` BIGINT);\n"),
                    output,
                    request.progress,
                )
                .await
                .map_err(DumpExecutorError::from)
        }
    }

    struct FakeAttestor;

    #[async_trait]
    impl LocalTargetAttestor for FakeAttestor {
        async fn attest(
            &self,
            _request: LocalTargetAttestationRequest<'_>,
        ) -> Result<LocalTargetAttestation, LocalTargetAttestationError> {
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

    struct FakeRestore;

    #[async_trait]
    impl RestoreExecutor for FakeRestore {
        async fn recreate_database(
            &self,
            _target: &AuthorizedLocalTarget,
            _metadata: &crate::domain::DumpArtifactMetadata,
        ) -> Result<(), RestoreExecutorError> {
            Ok(())
        }

        async fn import(
            &self,
            _target: &AuthorizedLocalTarget,
            artifact: &ValidatedRestoreArtifact,
        ) -> Result<RestoreMetrics, RestoreExecutorError> {
            Ok(RestoreMetrics::new_for_test(
                artifact.metadata().uncompressed_bytes,
            ))
        }
    }

    struct FakeWriter;

    #[async_trait]
    impl LocalTenantWriter for FakeWriter {
        async fn register(
            &self,
            _target: &AuthorizedLocalTarget,
            _registration: &crate::domain::LocalTenantRegistration,
        ) -> Result<(), LocalTenantWriteError> {
            Ok(())
        }
    }

    struct RecordingProgress(Arc<Mutex<Vec<PullProgress>>>);

    impl PullProgressObserver for RecordingProgress {
        fn update(&self, progress: PullProgress) {
            self.0.lock().unwrap().push(progress);
        }
    }

    struct TestDatabaseSelector(Option<DatabaseName>);

    impl PullDatabaseSelector for TestDatabaseSelector {
        fn select(
            &self,
            source_database: &DatabaseName,
        ) -> Result<DatabaseName, PullDatabaseSelectionError> {
            Ok(self.0.clone().unwrap_or_else(|| source_database.clone()))
        }
    }

    struct DefaultTargetSelector;

    impl PullTargetSelector for DefaultTargetSelector {
        fn select(
            &self,
            choices: &[PullTargetChoice],
        ) -> Result<ContainerName, PullTargetSelectionError> {
            choices
                .iter()
                .find(|choice| choice.is_default)
                .map(|choice| choice.container.clone())
                .ok_or(PullTargetSelectionError)
        }
    }

    async fn cached_fixture(root: &Path) -> (ConfigRepository, MemoryCredentialStore, DumpId) {
        let paths = AppPaths::new(root.join("config"), root.join("cache"), root.join("data"));
        let repository = ConfigRepository::new(paths.clone());
        let profile_name = ProfileName::try_from("salt-local").unwrap();
        let source_key = CredentialKey::new(CredentialScope::Source);
        let target_key = CredentialKey::new(CredentialScope::Target);
        let client = ClientCatalog::resolve("8.4").unwrap();
        let profile = SourceProfileConfig {
            host: "127.0.0.1".to_owned(),
            port: 3306,
            username: "root".to_owned(),
            credential_key: source_key,
            mysql_family: MysqlFamily::Mysql,
            mysql_series: "8.4".to_owned(),
            production: false,
            tls_mode: MysqlTlsMode::Disabled,
            client: MysqlClientConfig {
                image: client.image().to_owned(),
            },
            tenant_resolver: TenantResolverConfig::SaltCentral {
                central_database: DatabaseName::try_from("salt_central").unwrap(),
                allow_domain_lookup: true,
            },
        };
        let fingerprint = source_profile_fingerprint(&profile_name, &profile);
        repository
            .save(&AppConfig {
                active_profile: Some(profile_name.clone()),
                client_runtime: ClientRuntimeConfig {
                    docker_context: Some("desktop-linux".to_owned()),
                    ..ClientRuntimeConfig::default()
                },
                local_target: Some(LocalTargetConfig {
                    docker_context: "desktop-linux".to_owned(),
                    container_name: ContainerName::try_from("mysql-8").unwrap(),
                    container_id: ContainerId::try_from("a".repeat(64)).unwrap(),
                    username: "root".to_owned(),
                    credential_key: target_key,
                    central_database: DatabaseName::try_from("salt_central").unwrap(),
                    trust: LocalTargetTrust::UserConfirmed,
                    tenant_database_prefix: "salt_".to_owned(),
                }),
                profiles: BTreeMap::from([(profile_name.clone(), profile)]),
                ..AppConfig::default()
            })
            .unwrap();
        let credentials = MemoryCredentialStore::default();
        credentials
            .set(&target_key, SecretString::from("target-password"))
            .await
            .unwrap();

        let tenant_id = TenantId::try_from("salt_sagatec").unwrap();
        let stage = LocalArtifactStore::new(paths.cache_dir())
            .begin(&profile_name, &tenant_id)
            .unwrap();
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
                tenant_lookup: TenantLookup::try_from("sagatec").unwrap(),
                tenant_id,
                database: DatabaseName::try_from("salt_sagatec").unwrap(),
                profile: profile_name,
                source_fingerprint: fingerprint,
                source_server_uuid: "11111111-1111-4111-8111-111111111111".parse().unwrap(),
                source_version: "8.4.4".parse::<MysqlVersion>().unwrap(),
                client_version: client.version(),
                database_encoding: DatabaseEncoding::try_new(
                    "utf8mb4".to_owned(),
                    "utf8mb4_0900_ai_ci".to_owned(),
                )
                .unwrap(),
                local_tenant_features: LocalTenantFeatures::default(),
                policy_version: MYSQL_8_DUMP_POLICY_VERSION,
            },
            DumpArtifactCompletion {
                created_at_unix_seconds: 900,
                completed_at_unix_seconds: 901,
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
    async fn valid_cache_hit_restores_without_source_credential_or_source_calls() {
        let directory = tempdir().unwrap();
        let (repository, credentials, dump_id) = cached_fixture(directory.path()).await;
        let source_calls = Arc::new(AtomicUsize::new(0));
        let source = UnusedDump {
            calls: Arc::clone(&source_calls),
        };
        let progress = Arc::new(Mutex::new(Vec::new()));
        let service = PullService::with_clock(
            repository,
            TestClock(Arc::new(Mutex::new(VecDeque::from([1_000])))),
        )
        .with_progress(Arc::new(RecordingProgress(Arc::clone(&progress))));

        let ready = service
            .pull(
                &credentials,
                PullDumpDependencies {
                    tenant_resolver: &source,
                    preflight: &source,
                    executor: &source,
                },
                PullRestoreDependencies {
                    target_attestor: &FakeAttestor,
                    executor: FakeRestore,
                    tenant_writer: FakeWriter,
                    target_selector: &DefaultTargetSelector,
                    database_selector: &TestDatabaseSelector(None),
                },
                TenantLookup::try_from("sagatec").unwrap(),
                false,
            )
            .await
            .unwrap();

        assert_eq!(ready.cache, PullCacheUse::Hit { age_seconds: 99 });
        assert_eq!(ready.restored.plan.dump_id, dump_id);
        assert_eq!(source_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(matches!(
            progress.lock().unwrap().as_slice(),
            [
                PullProgress::CheckingCache,
                PullProgress::CacheHit {
                    dump_id: cached,
                    age_seconds: 99
                }
            ] if *cached == dump_id
        ));
    }

    #[tokio::test]
    async fn fresh_always_creates_and_restores_a_new_managed_dump() {
        let directory = tempdir().unwrap();
        let (repository, credentials, cached_dump_id) = cached_fixture(directory.path()).await;
        let source_key = repository
            .load()
            .unwrap()
            .profiles
            .values()
            .next()
            .unwrap()
            .credential_key;
        credentials
            .set(&source_key, SecretString::from("source-password"))
            .await
            .unwrap();
        let source_calls = Arc::new(AtomicUsize::new(0));
        let source = SuccessfulDump {
            calls: Arc::clone(&source_calls),
        };
        let service = PullService::with_clock(
            repository,
            TestClock(Arc::new(Mutex::new(VecDeque::from([
                1_000, 1_001, 1_002, 1_003,
            ])))),
        );

        let ready = service
            .pull(
                &credentials,
                PullDumpDependencies {
                    tenant_resolver: &source,
                    preflight: &source,
                    executor: &source,
                },
                PullRestoreDependencies {
                    target_attestor: &FakeAttestor,
                    executor: FakeRestore,
                    tenant_writer: FakeWriter,
                    target_selector: &DefaultTargetSelector,
                    database_selector: &TestDatabaseSelector(Some(
                        DatabaseName::try_from("salt_sagatec_debug").unwrap(),
                    )),
                },
                TenantLookup::try_from("sagatec").unwrap(),
                true,
            )
            .await
            .unwrap();

        assert_eq!(ready.cache, PullCacheUse::Created);
        assert_ne!(ready.restored.plan.dump_id, cached_dump_id);
        assert_eq!(ready.restored.plan.database.as_str(), "salt_sagatec_debug");
        assert_eq!(source_calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    }
}
