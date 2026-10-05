use async_trait::async_trait;
use secrecy::SecretString;
use thiserror::Error;

use crate::{
    application::restore_engine::versions_are_restore_compatible,
    domain::{
        DatabaseName, DumpArtifactMetadata, MysqlServerUuid, MysqlTlsMaterialPaths, MysqlTlsMode,
        MysqlVersion, ProfileName,
    },
    infrastructure::{
        artifact_store::{ArtifactStoreError, LocalArtifactStore},
        config::{AppConfig, ConfigError, ConfigRepository, SourceProfileConfig},
        credentials::{CredentialError, CredentialStore},
        mysql::{
            ApprovedMysqlClient, ClientCatalog, ClientCatalogError, DockerClientError,
            MysqlServerInfo,
        },
    },
};

/// A profile `push` may write to: explicitly allowed and never production.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteProfileChoice {
    pub profile: ProfileName,
    pub host: String,
    pub port: u16,
}

pub struct RemoteTargetProbeRequest<'a> {
    pub docker_context: &'a str,
    pub profile: &'a SourceProfileConfig,
    pub password: &'a SecretString,
    pub client: ApprovedMysqlClient,
}

#[async_trait]
pub trait RemoteTargetProbe: Send + Sync {
    async fn probe(
        &self,
        request: RemoteTargetProbeRequest<'_>,
    ) -> Result<MysqlServerInfo, DockerClientError>;
}

/// Turns a configured profile into a destination `push` may write to.
///
/// This is the only path that authorizes a write outside the local Docker
/// target, so every refusal happens here, before any SQL is sent.
pub struct RemoteTargetGate {
    repository: ConfigRepository,
}

impl RemoteTargetGate {
    pub fn new(repository: ConfigRepository) -> Self {
        Self { repository }
    }

    pub fn eligible_profiles(&self) -> Result<Vec<RemoteProfileChoice>, RemoteTargetGateError> {
        let config = self.repository.load()?;
        Ok(config
            .profiles
            .iter()
            .filter(|(name, profile)| {
                accepts_push(profile) && protected_endpoint(&config, name, profile).is_none()
            })
            .map(|(name, profile)| RemoteProfileChoice {
                profile: name.clone(),
                host: profile.host.clone(),
                port: profile.port,
            })
            .collect())
    }

    pub async fn verify(
        &self,
        credentials: &dyn CredentialStore,
        probe: &dyn RemoteTargetProbe,
        profile_name: &ProfileName,
    ) -> Result<GuardedRemoteTarget, RemoteTargetGateError> {
        let config = self.repository.load()?;
        let profile = config
            .profiles
            .get(profile_name)
            .ok_or(RemoteTargetGateError::ProfileNotFound)?;
        if profile.production {
            return Err(RemoteTargetGateError::ProductionDestination);
        }
        if !profile.push_destination {
            return Err(RemoteTargetGateError::PushNotAllowed {
                profile: profile_name.clone(),
            });
        }
        // A second profile for the same server that was never allowed (for
        // example the real production one) wins over the allowed copy.
        if let Some(protected) = protected_endpoint(&config, profile_name, profile) {
            return Err(RemoteTargetGateError::ProtectedEndpoint {
                profile: protected.clone(),
            });
        }
        let docker_context = config
            .client_runtime
            .docker_context
            .as_deref()
            .ok_or(RemoteTargetGateError::DockerContextMissing)?;
        let client = ClientCatalog::validate(&profile.mysql_series, &profile.client.image)?;
        let password = credentials.get(&profile.credential_key).await?;
        let server = probe
            .probe(RemoteTargetProbeRequest {
                docker_context,
                profile,
                password: &password,
                client,
            })
            .await?;
        if profile.tls_mode.requires_encrypted_transport() && server.tls_cipher.is_none() {
            return Err(RemoteTargetGateError::TlsRequiredButNotNegotiated);
        }
        if !server.vendor.to_ascii_lowercase().contains("mysql") {
            return Err(RemoteTargetGateError::UnsupportedVendor);
        }
        let detected_series = format!("{}.{}", server.version.major, server.version.minor);
        if detected_series != client.series() {
            return Err(RemoteTargetGateError::UnsupportedServerSeries);
        }
        // Hostnames can differ while the server is the same one. Any server a
        // protected profile was ever dumped from stays protected by identity.
        if let Some(protected) = self.protected_server(&config, &server.server_uuid)? {
            return Err(RemoteTargetGateError::ProtectedServer { profile: protected });
        }

        Ok(GuardedRemoteTarget {
            profile: profile_name.clone(),
            host: profile.host.clone(),
            port: profile.port,
            username: profile.username.clone(),
            password,
            tls_mode: profile.tls_mode,
            tls_material: profile.tls_material.clone(),
            docker_context: docker_context.to_owned(),
            client,
            server_version: server.version,
            server_uuid: server.server_uuid,
        })
    }

    /// Finds a cached dump that proves `server_uuid` belongs to a profile
    /// that does not accept pushes (or no longer exists).
    fn protected_server(
        &self,
        config: &AppConfig,
        server_uuid: &MysqlServerUuid,
    ) -> Result<Option<ProfileName>, RemoteTargetGateError> {
        let store = LocalArtifactStore::new(self.repository.paths().cache_dir());
        for located in store.list_all_candidates()? {
            let (_, _, artifact) = located.into_parts();
            let Ok(contents) = std::fs::read_to_string(&artifact.metadata_path) else {
                continue;
            };
            let Ok(metadata) = serde_json::from_str::<DumpArtifactMetadata>(&contents) else {
                continue;
            };
            if metadata.source_server_uuid == *server_uuid
                && !config
                    .profiles
                    .get(&metadata.profile)
                    .is_some_and(accepts_push)
            {
                return Ok(Some(metadata.profile));
            }
        }
        Ok(None)
    }

    /// Whether the server a dump came from is itself an allowed push
    /// destination. A source profile that was removed, never allowed, or is
    /// production protects its whole server.
    pub fn source_accepts_push(
        &self,
        metadata: &DumpArtifactMetadata,
    ) -> Result<bool, RemoteTargetGateError> {
        let config = self.repository.load()?;
        Ok(config
            .profiles
            .get(&metadata.profile)
            .is_some_and(accepts_push))
    }
}

const fn accepts_push(profile: &SourceProfileConfig) -> bool {
    profile.push_destination && !profile.production
}

/// Returns another profile that reaches the same host and port without
/// accepting pushes.
fn protected_endpoint<'a>(
    config: &'a AppConfig,
    name: &ProfileName,
    profile: &SourceProfileConfig,
) -> Option<&'a ProfileName> {
    config
        .profiles
        .iter()
        .find(|(candidate_name, candidate)| {
            *candidate_name != name
                && !accepts_push(candidate)
                && candidate.port == profile.port
                && endpoint_host(&candidate.host) == endpoint_host(&profile.host)
        })
        .map(|(name, _)| name)
}

/// Every spelling of "this machine" reaches the same server from the
/// Dockerized client, so they compare equal.
fn endpoint_host(host: &str) -> String {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    match host.as_str() {
        "localhost" | "::1" | "[::1]" | "host.docker.internal" => "loopback".to_owned(),
        _ if host.starts_with("127.") => "loopback".to_owned(),
        _ => host,
    }
}

pub struct GuardedRemoteTarget {
    profile: ProfileName,
    host: String,
    port: u16,
    username: String,
    password: SecretString,
    tls_mode: MysqlTlsMode,
    tls_material: MysqlTlsMaterialPaths,
    docker_context: String,
    client: ApprovedMysqlClient,
    server_version: MysqlVersion,
    server_uuid: MysqlServerUuid,
}

impl std::fmt::Debug for GuardedRemoteTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardedRemoteTarget")
            .field("profile", &self.profile)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("server_version", &self.server_version)
            .field("server_uuid", &self.server_uuid)
            .finish_non_exhaustive()
    }
}

impl GuardedRemoteTarget {
    pub const fn server_version(&self) -> MysqlVersion {
        self.server_version
    }

    /// Binds the destination database once the dump is known.
    ///
    /// The server a dump came from is refused outright unless its source
    /// profile also accepts pushes. Even then, the exact database the dump
    /// came from is refused; another database on that server is a copy.
    pub fn authorize(
        self,
        database: DatabaseName,
        metadata: &DumpArtifactMetadata,
        source_accepts_push: bool,
    ) -> Result<AuthorizedRemoteTarget, RemoteTargetGateError> {
        if self.server_uuid == metadata.source_server_uuid {
            if !source_accepts_push {
                return Err(RemoteTargetGateError::ProtectedSourceServer {
                    profile: metadata.profile.clone(),
                });
            }
            if database == metadata.database {
                return Err(RemoteTargetGateError::SourceCollision);
            }
        }
        if !versions_are_restore_compatible(
            metadata.source_version,
            metadata.client_version,
            self.server_version,
            self.client.version(),
        ) {
            return Err(RemoteTargetGateError::VersionMismatch);
        }
        Ok(AuthorizedRemoteTarget {
            target: self,
            database,
        })
    }
}

#[derive(Debug)]
pub struct AuthorizedRemoteTarget {
    target: GuardedRemoteTarget,
    database: DatabaseName,
}

impl AuthorizedRemoteTarget {
    pub fn profile(&self) -> &ProfileName {
        &self.target.profile
    }

    pub fn host(&self) -> &str {
        &self.target.host
    }

    pub const fn port(&self) -> u16 {
        self.target.port
    }

    pub fn username(&self) -> &str {
        &self.target.username
    }

    pub fn password(&self) -> &SecretString {
        &self.target.password
    }

    pub const fn tls_mode(&self) -> MysqlTlsMode {
        self.target.tls_mode
    }

    pub fn tls_material(&self) -> &MysqlTlsMaterialPaths {
        &self.target.tls_material
    }

    pub fn docker_context(&self) -> &str {
        &self.target.docker_context
    }

    pub const fn client(&self) -> ApprovedMysqlClient {
        self.target.client
    }

    pub const fn server_version(&self) -> MysqlVersion {
        self.target.server_version
    }

    /// The identity attested by the gate. Every write session re-checks it.
    pub fn server_uuid(&self) -> &MysqlServerUuid {
        &self.target.server_uuid
    }

    pub fn database(&self) -> &DatabaseName {
        &self.database
    }

    #[cfg(test)]
    pub(crate) fn for_test(database: DatabaseName) -> Self {
        Self {
            target: GuardedRemoteTarget {
                profile: ProfileName::try_from("sandbox").unwrap(),
                host: "sandbox.db.internal".to_owned(),
                port: 3306,
                username: "sandbox_writer".to_owned(),
                password: SecretString::from("remote-test-password"),
                tls_mode: MysqlTlsMode::Required,
                tls_material: MysqlTlsMaterialPaths::default(),
                docker_context: "desktop-linux".to_owned(),
                client: ClientCatalog::resolve("8.4").unwrap(),
                server_version: "8.4.4".parse().unwrap(),
                server_uuid: "33333333-3333-4333-8333-333333333333".parse().unwrap(),
            },
            database,
        }
    }
}

#[derive(Debug, Error)]
pub enum RemoteTargetGateError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error("the destination profile does not exist; see `reprodb profile list`")]
    ProfileNotFound,

    #[error("production profiles are never push destinations")]
    ProductionDestination,

    #[error(
        "profile `{profile}` does not accept pushes; allow it with `reprodb profile allow-push {profile}`"
    )]
    PushNotAllowed { profile: ProfileName },

    #[error(
        "the destination shares host and port with profile `{profile}`, which does not accept pushes; push refused"
    )]
    ProtectedEndpoint { profile: ProfileName },

    #[error("the MySQL client Docker context is missing; run `reprodb doctor`")]
    DockerContextMissing,

    #[error(transparent)]
    ClientCatalog(#[from] ClientCatalogError),

    #[error(transparent)]
    Credential(#[from] CredentialError),

    #[error(transparent)]
    Probe(#[from] DockerClientError),

    #[error(
        "the destination is the same MySQL server that profile `{profile}` was dumped from, and `{profile}` does not accept pushes; push refused"
    )]
    ProtectedServer { profile: ProfileName },

    #[error(transparent)]
    Store(#[from] ArtifactStoreError),

    #[error("the destination did not negotiate the TLS transport required by its profile")]
    TlsRequiredButNotNegotiated,

    #[error("the destination server is not MySQL")]
    UnsupportedVendor,

    #[error("the destination server series differs from its profile; re-add the profile")]
    UnsupportedServerSeries,

    #[error(
        "the destination is the server this dump came from (profile `{profile}`), which does not accept pushes; push refused for every database on it"
    )]
    ProtectedSourceServer { profile: ProfileName },

    #[error(
        "the destination is the server and database this dump came from; choose another database name"
    )]
    SourceCollision,

    #[error("the destination MySQL version cannot import this dump (downgrades are refused)")]
    VersionMismatch,
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex},
    };

    use secrecy::ExposeSecret as _;
    use tempfile::tempdir;

    use crate::{
        domain::{
            CredentialKey, CredentialScope, DatabaseEncoding, DumpArtifactCompletion,
            DumpArtifactContext, DumpId, Sha256Digest,
        },
        infrastructure::{
            config::{AppConfig, AppPaths, ClientRuntimeConfig, MysqlClientConfig, MysqlFamily},
            credentials::MemoryCredentialStore,
        },
    };

    use super::*;

    const SOURCE_UUID: &str = "11111111-1111-4111-8111-111111111111";
    const OTHER_UUID: &str = "33333333-3333-4333-8333-333333333333";

    struct FakeProbe {
        server: MysqlServerInfo,
        calls: Arc<Mutex<usize>>,
    }

    impl FakeProbe {
        fn returning(version: &str, server_uuid: &str, tls: bool) -> Self {
            Self {
                server: MysqlServerInfo {
                    version: version.parse().unwrap(),
                    vendor: "MySQL Community Server - GPL".to_owned(),
                    server_uuid: server_uuid.parse().unwrap(),
                    tls_cipher: tls.then(|| "TLS_AES_256_GCM_SHA384".to_owned()),
                },
                calls: Arc::new(Mutex::new(0)),
            }
        }

        fn calls(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    #[async_trait]
    impl RemoteTargetProbe for FakeProbe {
        async fn probe(
            &self,
            _request: RemoteTargetProbeRequest<'_>,
        ) -> Result<MysqlServerInfo, DockerClientError> {
            *self.calls.lock().unwrap() += 1;
            Ok(self.server.clone())
        }
    }

    fn profile(
        host: &str,
        production: bool,
        push_destination: bool,
        credential_key: CredentialKey,
    ) -> SourceProfileConfig {
        SourceProfileConfig {
            host: host.to_owned(),
            port: 3306,
            username: "writer".to_owned(),
            credential_key,
            mysql_family: MysqlFamily::Mysql,
            mysql_series: "8.4".to_owned(),
            production,
            push_destination,
            tls_mode: if production {
                MysqlTlsMode::VerifyIdentity
            } else {
                MysqlTlsMode::Required
            },
            tls_material: if production {
                MysqlTlsMaterialPaths {
                    ca: Some("/etc/reprodb/ca.pem".into()),
                    cert: None,
                    key: None,
                }
            } else {
                MysqlTlsMaterialPaths::default()
            },
            client: MysqlClientConfig {
                image: ClientCatalog::resolve("8.4").unwrap().image().to_owned(),
            },
        }
    }

    /// - `sandbox`: allowed push destination.
    /// - `prod-source`: flagged production.
    /// - `rds-main`: real production server **not** flagged production (a
    ///   mislabeled profile), never allowed.
    /// - `disguised`: allowed, but same endpoint as `rds-main`.
    /// - `dev`: non-production, never allowed.
    /// - `local-prod` / `local-alias`: the same loopback server spelled two ways;
    ///   only the alias is allowed.
    async fn fixture(root: &std::path::Path) -> (ConfigRepository, MemoryCredentialStore) {
        let repository = ConfigRepository::new(AppPaths::new(
            root.join("config"),
            root.join("cache"),
            root.join("data"),
        ));
        let profiles = [
            ("sandbox", "sandbox.db.internal", false, true),
            ("prod-source", "prod.db.internal", true, false),
            ("rds-main", "rds-main.example.com", false, false),
            ("disguised", "RDS-MAIN.example.com", false, true),
            ("dev", "dev.db.internal", false, false),
            ("local-prod", "127.0.0.1", false, false),
            ("local-alias", "localhost", false, true),
        ];
        let credentials = MemoryCredentialStore::default();
        let mut configured = BTreeMap::new();
        for (name, host, production, push) in profiles {
            let key = CredentialKey::new(CredentialScope::Source);
            credentials
                .set(&key, SecretString::from(format!("{name}-password")))
                .await
                .unwrap();
            configured.insert(
                ProfileName::try_from(name).unwrap(),
                profile(host, production, push, key),
            );
        }
        repository
            .save(&AppConfig {
                client_runtime: ClientRuntimeConfig {
                    docker_context: Some("desktop-linux".to_owned()),
                    ..ClientRuntimeConfig::default()
                },
                profiles: configured,
                ..AppConfig::default()
            })
            .unwrap();
        (repository, credentials)
    }

    fn metadata(source_profile: &str, source_version: &str) -> DumpArtifactMetadata {
        DumpArtifactMetadata::try_new(
            DumpId::new(),
            DumpArtifactContext {
                database: DatabaseName::try_from("salt_sagatec").unwrap(),
                profile: ProfileName::try_from(source_profile).unwrap(),
                source_fingerprint: Sha256Digest::from_bytes([1; 32]),
                source_server_uuid: SOURCE_UUID.parse().unwrap(),
                source_version: source_version.parse().unwrap(),
                client_version: source_version.parse().unwrap(),
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
                uncompressed_bytes: 10,
                compressed_bytes: 10,
                sql_sha256: Sha256Digest::from_bytes([2; 32]),
                artifact_sha256: Sha256Digest::from_bytes([3; 32]),
            },
        )
        .unwrap()
    }

    fn name(value: &str) -> ProfileName {
        ProfileName::try_from(value).unwrap()
    }

    fn database(value: &str) -> DatabaseName {
        DatabaseName::try_from(value).unwrap()
    }

    async fn verify(
        profile: &str,
        probe: &FakeProbe,
    ) -> Result<GuardedRemoteTarget, RemoteTargetGateError> {
        let directory = tempdir().unwrap();
        let (repository, credentials) = fixture(directory.path()).await;
        RemoteTargetGate::new(repository)
            .verify(&credentials, probe, &name(profile))
            .await
    }

    #[tokio::test]
    async fn only_explicitly_allowed_profiles_on_unprotected_endpoints_are_offered() {
        let directory = tempdir().unwrap();
        let (repository, _credentials) = fixture(directory.path()).await;

        let choices = RemoteTargetGate::new(repository)
            .eligible_profiles()
            .unwrap();

        assert_eq!(
            choices,
            vec![RemoteProfileChoice {
                profile: name("sandbox"),
                host: "sandbox.db.internal".to_owned(),
                port: 3306,
            }]
        );
    }

    #[tokio::test]
    async fn a_production_profile_is_refused_before_reading_credentials_or_connecting() {
        let probe = FakeProbe::returning("8.4.4", OTHER_UUID, true);

        let error = verify("prod-source", &probe).await.unwrap_err();

        assert!(matches!(
            error,
            RemoteTargetGateError::ProductionDestination
        ));
        assert_eq!(probe.calls(), 0);
    }

    #[tokio::test]
    async fn a_profile_never_allowed_is_refused_even_when_not_flagged_production() {
        let probe = FakeProbe::returning("8.4.4", OTHER_UUID, true);

        for profile in ["rds-main", "dev"] {
            let error = verify(profile, &probe).await.unwrap_err();
            assert!(
                matches!(error, RemoteTargetGateError::PushNotAllowed { profile: ref refused } if refused.as_str() == profile)
            );
            assert!(error.to_string().contains("reprodb profile allow-push"));
        }
        assert_eq!(probe.calls(), 0);
    }

    #[tokio::test]
    async fn an_allowed_profile_sharing_an_endpoint_with_a_protected_one_is_refused() {
        let probe = FakeProbe::returning("8.4.4", OTHER_UUID, true);

        let error = verify("disguised", &probe).await.unwrap_err();

        assert!(matches!(
            error,
            RemoteTargetGateError::ProtectedEndpoint { ref profile } if profile.as_str() == "rds-main"
        ));
        assert_eq!(probe.calls(), 0);
    }

    #[tokio::test]
    async fn an_unknown_profile_is_refused() {
        let probe = FakeProbe::returning("8.4.4", OTHER_UUID, true);

        let error = verify("missing", &probe).await.unwrap_err();

        assert!(matches!(error, RemoteTargetGateError::ProfileNotFound));
    }

    #[tokio::test]
    async fn the_selected_profile_and_its_own_credential_are_bound_to_the_target() {
        let target = verify("sandbox", &FakeProbe::returning("8.4.4", OTHER_UUID, true))
            .await
            .unwrap()
            .authorize(
                database("salt_sagatec"),
                &metadata("rds-main", "8.4.4"),
                false,
            )
            .unwrap();

        assert_eq!(target.profile().as_str(), "sandbox");
        assert_eq!(target.host(), "sandbox.db.internal");
        assert_eq!(target.password().expose_secret(), "sandbox-password");
        assert_eq!(target.server_uuid(), &OTHER_UUID.parse().unwrap());
    }

    #[tokio::test]
    async fn a_required_tls_profile_without_a_cipher_is_refused() {
        let error = verify("sandbox", &FakeProbe::returning("8.4.4", OTHER_UUID, false))
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            RemoteTargetGateError::TlsRequiredButNotNegotiated
        ));
    }

    #[tokio::test]
    async fn a_server_outside_the_profile_series_is_refused() {
        let error = verify("sandbox", &FakeProbe::returning("8.0.45", OTHER_UUID, true))
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            RemoteTargetGateError::UnsupportedServerSeries
        ));
    }

    #[tokio::test]
    async fn the_server_a_protected_dump_came_from_is_refused_for_every_database() {
        // The sandbox profile resolves to the very server `rds-main` was dumped from.
        let error = verify("sandbox", &FakeProbe::returning("8.4.4", SOURCE_UUID, true))
            .await
            .unwrap()
            .authorize(
                database("salt_sagatec_qa"),
                &metadata("rds-main", "8.4.4"),
                false,
            )
            .unwrap_err();

        assert!(matches!(
            error,
            RemoteTargetGateError::ProtectedSourceServer { ref profile } if profile.as_str() == "rds-main"
        ));
    }

    #[tokio::test]
    async fn on_an_allowed_source_server_only_the_source_database_is_refused() {
        let probe = FakeProbe::returning("8.4.4", SOURCE_UUID, true);

        let same = verify("sandbox", &probe)
            .await
            .unwrap()
            .authorize(
                database("salt_sagatec"),
                &metadata("sandbox", "8.4.4"),
                true,
            )
            .unwrap_err();
        let other = verify("sandbox", &probe)
            .await
            .unwrap()
            .authorize(
                database("salt_sagatec_qa"),
                &metadata("sandbox", "8.4.4"),
                true,
            )
            .unwrap();

        assert!(matches!(same, RemoteTargetGateError::SourceCollision));
        assert_eq!(other.database().as_str(), "salt_sagatec_qa");
    }

    #[tokio::test]
    async fn only_an_existing_allowed_source_profile_unlocks_its_server() {
        let directory = tempdir().unwrap();
        let (repository, _credentials) = fixture(directory.path()).await;
        let gate = RemoteTargetGate::new(repository);

        for (profile, expected) in [
            ("sandbox", true),
            ("prod-source", false),
            ("rds-main", false),
            ("dev", false),
            ("removed-profile", false),
        ] {
            assert_eq!(
                gate.source_accepts_push(&metadata(profile, "8.4.4"))
                    .unwrap(),
                expected,
                "{profile}"
            );
        }
    }

    #[tokio::test]
    async fn a_downgrade_is_refused() {
        let error = verify("sandbox", &FakeProbe::returning("8.4.4", OTHER_UUID, true))
            .await
            .unwrap()
            .authorize(
                database("salt_sagatec"),
                &metadata("sandbox", "9.1.0"),
                true,
            )
            .unwrap_err();

        assert!(matches!(error, RemoteTargetGateError::VersionMismatch));
    }

    #[tokio::test]
    async fn loopback_spellings_are_the_same_endpoint() {
        let probe = FakeProbe::returning("8.4.4", OTHER_UUID, true);

        let error = verify("local-alias", &probe).await.unwrap_err();

        assert!(matches!(
            error,
            RemoteTargetGateError::ProtectedEndpoint { ref profile } if profile.as_str() == "local-prod"
        ));
        assert_eq!(endpoint_host("LOCALHOST."), endpoint_host("127.0.0.2"));
        assert_ne!(
            endpoint_host("sandbox.db.internal"),
            endpoint_host("localhost")
        );
    }

    #[tokio::test]
    async fn a_server_seen_in_a_protected_profiles_dump_is_refused_under_any_hostname() {
        let directory = tempdir().unwrap();
        let (repository, credentials) = fixture(directory.path()).await;
        // `local-source` (not configured, so not allowed) was dumped from SOURCE_UUID.
        let _artifact = crate::infrastructure::restore_artifact::test_support::validated_artifact(
            repository.paths().cache_dir(),
            b"SELECT 1;\n",
        )
        .await;
        let probe = FakeProbe::returning("8.4.4", SOURCE_UUID, true);

        let error = RemoteTargetGate::new(repository)
            .verify(&credentials, &probe, &name("sandbox"))
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            RemoteTargetGateError::ProtectedServer { ref profile } if profile.as_str() == "local-source"
        ));
    }
}
