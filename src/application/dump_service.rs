use std::{path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use secrecy::SecretString;
use thiserror::Error;

use crate::{
    domain::{
        DatabaseName, DumpArtifactCompletion, DumpArtifactContext, DumpArtifactMetadata, DumpId,
        DumpMetadataError, DumpPolicyNotice, MysqlVersion, ProfileName, ResolvedTenant,
        TenantLookup, TenantResolutionError,
    },
    infrastructure::{
        artifact_store::{ArtifactStoreError, LocalArtifactStore},
        cache_cleanup::{CacheCleanupError, CacheCleanupPolicy, LocalCacheCleaner},
        compression::{CompressionProgressObserver, NoCompressionProgress},
        config::{ConfigError, ConfigRepository, SourceProfileConfig, source_profile_fingerprint},
        credentials::{CredentialError, CredentialStore},
        mysql::{
            ApprovedMysqlClient, ApprovedMysqlDump, ClientCatalog, ClientCatalogError,
            DumpExecutionRequest, DumpExecutor, DumpExecutorError, DumpPreflightError,
        },
        operation_lock::{OperationLockError, OperationLockKey, OperationLockManager},
    },
};

pub struct DumpSource<'a> {
    pub profile_name: &'a ProfileName,
    pub docker_context: &'a str,
    pub profile: &'a SourceProfileConfig,
    pub password: &'a SecretString,
    pub client: ApprovedMysqlClient,
}

#[async_trait]
pub trait DumpTenantResolver: Send + Sync {
    async fn resolve(
        &self,
        source: &DumpSource<'_>,
        lookup: &TenantLookup,
    ) -> Result<ResolvedTenant, TenantResolutionError>;
}

#[async_trait]
pub trait DumpPreflightGateway: Send + Sync {
    async fn assess(
        &self,
        source: &DumpSource<'_>,
        database: &DatabaseName,
    ) -> Result<ApprovedMysqlDump, DumpPreflightError>;
}

pub trait Clock: Send + Sync {
    fn now_unix_seconds(&self) -> Result<u64, ClockError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix_seconds(&self) -> Result<u64, ClockError> {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .map_err(|_| ClockError)
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("the system clock is before the Unix epoch")]
pub struct ClockError;

pub struct DumpService<C = SystemClock> {
    repository: ConfigRepository,
    clock: C,
    progress: Arc<dyn CompressionProgressObserver>,
}

impl DumpService<SystemClock> {
    pub fn new(repository: ConfigRepository) -> Self {
        Self {
            repository,
            clock: SystemClock,
            progress: Arc::new(NoCompressionProgress),
        }
    }
}

impl<C> DumpService<C>
where
    C: Clock,
{
    pub fn with_clock(repository: ConfigRepository, clock: C) -> Self {
        Self {
            repository,
            clock,
            progress: Arc::new(NoCompressionProgress),
        }
    }

    pub fn with_progress(mut self, progress: Arc<dyn CompressionProgressObserver>) -> Self {
        self.progress = progress;
        self
    }

    pub async fn create(
        &self,
        credentials: &dyn CredentialStore,
        resolver: &dyn DumpTenantResolver,
        preflight: &dyn DumpPreflightGateway,
        executor: &dyn DumpExecutor,
        lookup: TenantLookup,
    ) -> Result<DumpCreated, DumpServiceError> {
        let cleanup_now = self.clock.now_unix_seconds()?;
        LocalCacheCleaner::new(self.repository.paths().cache_dir())
            .clean(CacheCleanupPolicy::defaults_at(cleanup_now))?;

        let config = self.repository.load()?;
        let profile_name = config
            .active_profile
            .as_ref()
            .ok_or(DumpServiceError::NoActiveProfile)?;
        let profile = config
            .profiles
            .get(profile_name)
            .ok_or(DumpServiceError::NoActiveProfile)?;
        if profile.production {
            return Err(DumpServiceError::ProductionNotEnabled);
        }
        let docker_context = config
            .client_runtime
            .docker_context
            .as_deref()
            .ok_or(DumpServiceError::DockerContextMissing)?;
        let client = ClientCatalog::validate(&profile.mysql_series, &profile.client.image)?;
        let password = credentials.get(&profile.credential_key).await?;
        let source = DumpSource {
            profile_name,
            docker_context,
            profile,
            password: &password,
            client,
        };
        let resolved = resolver.resolve(&source, &lookup).await?;
        let lock_manager = OperationLockManager::new(self.repository.paths().cache_dir());
        let _lock =
            lock_manager.try_acquire(OperationLockKey::source(profile_name, &resolved.database))?;
        let approved = preflight.assess(&source, &resolved.database).await?;

        let created_at = self.clock.now_unix_seconds()?;
        let store = LocalArtifactStore::new(self.repository.paths().cache_dir());
        let stage = store.begin(profile_name, &resolved.tenant_id)?;
        let dump_id = stage.dump_id();
        let output = stage.create_dump_writer()?;
        let metrics = executor
            .execute(
                DumpExecutionRequest {
                    profile_name,
                    docker_context,
                    profile,
                    password: &password,
                    client,
                    plan: &approved.plan,
                    progress: Arc::clone(&self.progress),
                },
                output,
            )
            .await?;
        let completed_at = self.clock.now_unix_seconds()?;
        let metadata = DumpArtifactMetadata::try_new(
            dump_id,
            DumpArtifactContext {
                tenant_lookup: lookup.clone(),
                tenant_id: resolved.tenant_id.clone(),
                database: resolved.database.clone(),
                profile: profile_name.clone(),
                source_fingerprint: source_profile_fingerprint(profile_name, profile),
                source_server_uuid: approved.server.server_uuid.clone(),
                source_version: approved.server.version,
                client_version: client.version(),
                database_encoding: approved.preflight.encoding.clone(),
                local_tenant_features: resolved.features.clone(),
                policy_version: approved.plan.policy_version(),
            },
            DumpArtifactCompletion {
                created_at_unix_seconds: created_at,
                completed_at_unix_seconds: completed_at,
                uncompressed_bytes: metrics.input_bytes(),
                compressed_bytes: metrics.compressed_bytes(),
                sql_sha256: metrics.input_sha256(),
                artifact_sha256: metrics.compressed_sha256(),
            },
        )?;
        let artifact = stage.publish(&metadata, &metrics)?;

        Ok(DumpCreated {
            dump_id,
            artifact_path: artifact.path().to_owned(),
            profile: profile_name.clone(),
            tenant_lookup: lookup,
            tenant_id: resolved.tenant_id,
            database: resolved.database,
            source_version: approved.server.version,
            client_version: client.version(),
            uncompressed_bytes: metrics.input_bytes(),
            compressed_bytes: metrics.compressed_bytes(),
            elapsed: metrics.elapsed(),
            notices: approved.plan.notices().to_vec(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DumpCreated {
    pub dump_id: DumpId,
    pub artifact_path: PathBuf,
    pub profile: ProfileName,
    pub tenant_lookup: TenantLookup,
    pub tenant_id: crate::domain::TenantId,
    pub database: DatabaseName,
    pub source_version: MysqlVersion,
    pub client_version: MysqlVersion,
    pub uncompressed_bytes: u64,
    pub compressed_bytes: u64,
    pub elapsed: Duration,
    pub notices: Vec<DumpPolicyNotice>,
}

#[derive(Debug, Error)]
pub enum DumpServiceError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error(
        "no active source profile; run `reprodb profile add NAME` or `reprodb profile use NAME`"
    )]
    NoActiveProfile,

    #[error("the MySQL client Docker context is missing; run `reprodb doctor`")]
    DockerContextMissing,

    #[error("production profiles are not enabled until production hardening is complete")]
    ProductionNotEnabled,

    #[error(transparent)]
    ClientCatalog(#[from] ClientCatalogError),

    #[error(transparent)]
    Credential(#[from] CredentialError),

    #[error(transparent)]
    Tenant(#[from] TenantResolutionError),

    #[error(transparent)]
    Lock(#[from] OperationLockError),

    #[error(transparent)]
    Preflight(#[from] DumpPreflightError),

    #[error(transparent)]
    Execute(#[from] DumpExecutorError),

    #[error(transparent)]
    Artifact(#[from] ArtifactStoreError),

    #[error(transparent)]
    Metadata(#[from] DumpMetadataError),

    #[error(transparent)]
    Cleanup(#[from] CacheCleanupError),

    #[error(transparent)]
    Clock(#[from] ClockError),
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, VecDeque},
        io::Cursor,
        sync::Mutex,
    };

    use secrecy::SecretString;
    use tempfile::tempdir;

    use crate::{
        domain::{
            AppColor, DatabaseEncoding, DatabaseObjectCounts, DefinerObjectCounts, DumpPreflight,
            GtidMode, LocalTenantFeatures, Mysql8DumpPolicy, MysqlTlsMode, StorageEngineUsage,
            TenantId, TenantMatch,
        },
        infrastructure::{
            compression::ZstdCompressor,
            config::{
                AppConfig, AppPaths, ClientRuntimeConfig, ClientRuntimeKind, MysqlClientConfig,
                MysqlFamily, TenantResolverConfig,
            },
            credentials::MemoryCredentialStore,
            mysql::{DumpFailureKind, MysqlServerInfo},
        },
    };

    use super::*;

    struct TestClock {
        values: Mutex<VecDeque<u64>>,
    }

    impl TestClock {
        fn new(values: impl IntoIterator<Item = u64>) -> Self {
            Self {
                values: Mutex::new(values.into_iter().collect()),
            }
        }
    }

    impl Clock for TestClock {
        fn now_unix_seconds(&self) -> Result<u64, ClockError> {
            self.values.lock().unwrap().pop_front().ok_or(ClockError)
        }
    }

    struct FakeResolver;

    #[async_trait]
    impl DumpTenantResolver for FakeResolver {
        async fn resolve(
            &self,
            _source: &DumpSource<'_>,
            lookup: &TenantLookup,
        ) -> Result<ResolvedTenant, TenantResolutionError> {
            assert_eq!(lookup.as_str(), "sagatec");
            Ok(ResolvedTenant {
                tenant_id: TenantId::try_from("salt_sagatec").unwrap(),
                database: DatabaseName::try_from("salt_sagatec").unwrap(),
                matched_by: TenantMatch::TenantId,
                features: LocalTenantFeatures {
                    app_color: Some(AppColor::try_from("green".to_owned()).unwrap()),
                    enable_beta: Some(true),
                    ..LocalTenantFeatures::default()
                },
            })
        }
    }

    struct FakePreflight;

    #[async_trait]
    impl DumpPreflightGateway for FakePreflight {
        async fn assess(
            &self,
            _source: &DumpSource<'_>,
            database: &DatabaseName,
        ) -> Result<ApprovedMysqlDump, DumpPreflightError> {
            let preflight = safe_preflight();
            let version = "8.4.4".parse::<MysqlVersion>().unwrap();
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

    struct FakeExecutor {
        fail: bool,
    }

    #[async_trait]
    impl DumpExecutor for FakeExecutor {
        async fn execute(
            &self,
            request: DumpExecutionRequest<'_>,
            output: std::fs::File,
        ) -> Result<crate::infrastructure::compression::CompressionMetrics, DumpExecutorError>
        {
            if self.fail {
                return Err(DumpExecutorError::ProcessFailed {
                    exit_code: Some(2),
                    kind: DumpFailureKind::Unknown,
                    stderr_truncated: false,
                });
            }
            assert_eq!(request.profile_name.as_str(), "local-source");
            assert_eq!(request.plan.arguments().last().unwrap(), "salt_sagatec");
            ZstdCompressor::default()
                .compress(
                    Cursor::new(b"CREATE TABLE example (id BIGINT);\n"),
                    output,
                    request.progress,
                )
                .await
                .map_err(DumpExecutorError::from)
        }
    }

    fn safe_preflight() -> DumpPreflight {
        DumpPreflight {
            encoding: DatabaseEncoding::try_new(
                "utf8mb4".to_owned(),
                "utf8mb4_0900_ai_ci".to_owned(),
            )
            .unwrap(),
            engines: vec![StorageEngineUsage::try_new("InnoDB".to_owned(), 1).unwrap()],
            objects: DatabaseObjectCounts::default(),
            definers: DefinerObjectCounts::default(),
            gtid_mode: GtidMode::On,
        }
    }

    fn repository(
        root: &std::path::Path,
        production: bool,
    ) -> (ConfigRepository, crate::domain::CredentialKey) {
        let paths = AppPaths::new(root.join("config"), root.join("cache"), root.join("data"));
        let repository = ConfigRepository::new(paths);
        let profile_name = ProfileName::try_from("local-source").unwrap();
        let credential_key =
            crate::domain::CredentialKey::new(crate::domain::CredentialScope::Source);
        let client = ClientCatalog::resolve("8.4").unwrap();
        let mut profiles = BTreeMap::new();
        profiles.insert(
            profile_name.clone(),
            SourceProfileConfig {
                host: "127.0.0.1".to_owned(),
                port: 3306,
                username: "root".to_owned(),
                credential_key,
                mysql_family: MysqlFamily::Mysql,
                mysql_series: "8.4".to_owned(),
                production,
                tls_mode: if production {
                    MysqlTlsMode::Required
                } else {
                    MysqlTlsMode::Disabled
                },
                client: MysqlClientConfig {
                    image: client.image().to_owned(),
                },
                tenant_resolver: TenantResolverConfig::Pattern {
                    pattern: "salt_{tenant}".to_owned(),
                },
            },
        );
        repository
            .save(&AppConfig {
                active_profile: Some(profile_name),
                client_runtime: ClientRuntimeConfig {
                    kind: ClientRuntimeKind::Docker,
                    docker_context: Some("desktop-linux".to_owned()),
                },
                profiles,
                ..AppConfig::default()
            })
            .unwrap();
        (repository, credential_key)
    }

    #[tokio::test]
    async fn creates_and_publishes_a_complete_managed_dump() {
        let directory = tempdir().unwrap();
        let (repository, credential_key) = repository(directory.path(), false);
        let credentials = MemoryCredentialStore::default();
        credentials
            .set(&credential_key, SecretString::from("local-password"))
            .await
            .unwrap();
        let cache_root = repository.paths().cache_dir().to_owned();
        let service = DumpService::with_clock(repository, TestClock::new([900, 1_000, 1_001]));

        let created = service
            .create(
                &credentials,
                &FakeResolver,
                &FakePreflight,
                &FakeExecutor { fail: false },
                TenantLookup::try_from("sagatec").unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(created.profile.as_str(), "local-source");
        assert_eq!(created.tenant_id.as_str(), "salt_sagatec");
        assert_eq!(created.database.as_str(), "salt_sagatec");
        assert!(created.artifact_path.is_dir());
        assert!(created.compressed_bytes > 0);
        let artifacts = LocalArtifactStore::new(&cache_root)
            .list_complete(&created.profile, &created.tenant_id)
            .unwrap();
        assert_eq!(artifacts.len(), 1);
        let sql = zstd::stream::decode_all(std::fs::File::open(artifacts[0].dump_path()).unwrap())
            .unwrap();
        assert_eq!(sql, b"CREATE TABLE example (id BIGINT);\n");
        let metadata: crate::domain::DumpArtifactMetadata =
            serde_json::from_reader(std::fs::File::open(artifacts[0].metadata_path()).unwrap())
                .unwrap();
        assert_eq!(metadata.local_tenant_features.enable_beta, Some(true));
        assert_eq!(
            metadata.local_tenant_features.app_color.unwrap().as_str(),
            "green"
        );

        let lock = OperationLockManager::new(cache_root).try_acquire(OperationLockKey::source(
            &created.profile,
            &created.database,
        ));
        assert!(lock.is_ok(), "dump lock must be released after success");
    }

    #[tokio::test]
    async fn executor_failure_removes_the_partial_artifact() {
        let directory = tempdir().unwrap();
        let (repository, credential_key) = repository(directory.path(), false);
        let credentials = MemoryCredentialStore::default();
        credentials
            .set(&credential_key, SecretString::from("local-password"))
            .await
            .unwrap();
        let cache_root = repository.paths().cache_dir().to_owned();
        let service = DumpService::with_clock(repository, TestClock::new([900, 1_000]));

        let error = service
            .create(
                &credentials,
                &FakeResolver,
                &FakePreflight,
                &FakeExecutor { fail: true },
                TenantLookup::try_from("sagatec").unwrap(),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, DumpServiceError::Execute(_)));
        assert!(
            LocalArtifactStore::new(cache_root)
                .list_complete(
                    &ProfileName::try_from("local-source").unwrap(),
                    &TenantId::try_from("salt_sagatec").unwrap(),
                )
                .unwrap()
                .is_empty()
        );
        let tenant_cache = directory
            .path()
            .join("cache/profiles/local-source/salt_sagatec");
        let remaining_entries = std::fs::read_dir(&tenant_cache)
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().file_name())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        assert!(
            remaining_entries
                .iter()
                .all(|name| !name.to_string_lossy().ends_with(".part")),
            "failed dumps must not leave a staged artifact behind"
        );
    }

    #[tokio::test]
    async fn production_profile_is_blocked_before_credential_access() {
        let directory = tempdir().unwrap();
        let (repository, _) = repository(directory.path(), true);
        let service = DumpService::with_clock(repository, TestClock::new([900]));

        let error = service
            .create(
                &MemoryCredentialStore::default(),
                &FakeResolver,
                &FakePreflight,
                &FakeExecutor { fail: false },
                TenantLookup::try_from("sagatec").unwrap(),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, DumpServiceError::ProductionNotEnabled));
    }
}
