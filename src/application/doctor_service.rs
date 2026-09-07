use std::path::Path;

use async_trait::async_trait;

use crate::{
    domain::{ContainerId, ContainerName},
    infrastructure::{
        config::{ConfigRepository, SourceProfileConfig},
        credentials::{CredentialError, CredentialStore},
    },
};

use super::{
    LocalTargetVerifier, NewLocalTargetInput, NewProfileInput, SourceProfileVerifier,
    SourceVerificationError, TargetVerificationError,
};

pub const MINIMUM_FREE_BYTES: u64 = 5 * 1024 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DoctorSection {
    Configuration,
    Storage,
    Docker,
    Source,
    Target,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DoctorStatus {
    Passed,
    Warning,
    Failed,
    Skipped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DoctorFailureKind {
    Configuration,
    Credential,
    Dependency,
    SourceConnection,
    Docker,
    Filesystem,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorCheck {
    pub section: DoctorSection,
    pub label: &'static str,
    pub status: DoctorStatus,
    pub detail: String,
    pub action: Option<&'static str>,
    pub failure_kind: Option<DoctorFailureKind>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DoctorReport {
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    pub fn first_failure_kind(&self) -> Option<DoctorFailureKind> {
        const ORDER: [DoctorSection; 5] = [
            DoctorSection::Configuration,
            DoctorSection::Storage,
            DoctorSection::Docker,
            DoctorSection::Source,
            DoctorSection::Target,
        ];

        ORDER.into_iter().find_map(|section| {
            self.checks
                .iter()
                .filter(|check| check.section == section)
                .find_map(|check| check.failure_kind)
        })
    }

    pub fn is_ready(&self) -> bool {
        self.first_failure_kind().is_none()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorDockerContainer {
    pub id: ContainerId,
    pub name: ContainerName,
    pub running: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorDockerInventory {
    pub context: String,
    pub containers: Vec<DoctorDockerContainer>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DoctorDockerError {
    Unavailable,
    RemoteContext,
    InvalidMetadata,
}

#[async_trait]
pub trait DoctorDockerInspector: Send + Sync {
    async fn inspect(&self) -> Result<DoctorDockerInventory, DoctorDockerError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DoctorStorageError {
    Unavailable,
}

pub trait DoctorStorageInspector: Send + Sync {
    fn available_bytes(&self, path: &Path) -> Result<u64, DoctorStorageError>;
}

pub struct DoctorService {
    repository: ConfigRepository,
}

impl DoctorService {
    pub fn new(repository: ConfigRepository) -> Self {
        Self { repository }
    }

    pub async fn run(
        &self,
        credentials: &dyn CredentialStore,
        docker: &dyn DoctorDockerInspector,
        storage: &dyn DoctorStorageInspector,
        source_verifier: &dyn SourceProfileVerifier,
        target_verifier: &dyn LocalTargetVerifier,
    ) -> DoctorReport {
        let mut report = DoctorReport::default();
        let config = match self.repository.load() {
            Ok(config) => {
                let exists = self.repository.paths().config_file().exists();
                report.checks.push(if exists {
                    passed(
                        DoctorSection::Configuration,
                        "configuration",
                        "configuration loaded and validated",
                    )
                } else {
                    warning(
                        DoctorSection::Configuration,
                        "configuration",
                        "configuration file has not been created yet",
                        "Run `reprodb setup` and `reprodb profile add NAME`.",
                    )
                });
                Some(config)
            }
            Err(_) => {
                report.checks.push(failed(
                    DoctorSection::Configuration,
                    "configuration",
                    "configuration could not be loaded or validated",
                    "Review the TOML or recreate it with `reprodb setup`.",
                    DoctorFailureKind::Configuration,
                ));
                None
            }
        };

        match storage.available_bytes(self.repository.paths().cache_dir()) {
            Ok(bytes) if bytes >= MINIMUM_FREE_BYTES => report.checks.push(passed(
                DoctorSection::Storage,
                "cache filesystem",
                format!("{} available for compressed dumps", format_gib(bytes)),
            )),
            Ok(bytes) => report.checks.push(failed(
                DoctorSection::Storage,
                "cache filesystem",
                format!(
                    "only {} available; reprodb requires at least 5.0 GiB",
                    format_gib(bytes)
                ),
                "Free local disk space before creating a dump.",
                DoctorFailureKind::Filesystem,
            )),
            Err(DoctorStorageError::Unavailable) => report.checks.push(failed(
                DoctorSection::Storage,
                "cache filesystem",
                "available space could not be determined for the cache filesystem",
                "Check permissions and availability of the local cache volume.",
                DoctorFailureKind::Filesystem,
            )),
        }

        let active = config.as_ref().and_then(|config| {
            config.active_profile.as_ref().and_then(|name| {
                config
                    .profiles
                    .get(name)
                    .cloned()
                    .map(|profile| (name.clone(), profile))
            })
        });
        report.checks.push(match &active {
            Some((name, profile)) => passed(
                DoctorSection::Source,
                "active profile",
                format!(
                    "{} · {}:{} · MySQL {} · TLS {}",
                    name, profile.host, profile.port, profile.mysql_series, profile.tls_mode
                ),
            ),
            None => failed(
                DoctorSection::Source,
                "active profile",
                "no source profile is active",
                "Run `reprodb profile add NAME` or `reprodb profile use NAME`.",
                DoctorFailureKind::Configuration,
            ),
        });

        let target = config
            .as_ref()
            .and_then(|config| config.local_target.clone());
        report.checks.push(match &target {
            Some(target) => passed(
                DoctorSection::Target,
                "local target configuration",
                format!(
                    "{} · context {} · central database {} · allowlist {}* · {}",
                    target.container_name,
                    target.docker_context,
                    target.central_database,
                    target.tenant_database_prefix,
                    match target.trust {
                        crate::infrastructure::config::LocalTargetTrust::ReprodbManaged => {
                            "reprodb-managed"
                        }
                        crate::infrastructure::config::LocalTargetTrust::UserConfirmed => {
                            "user-confirmed"
                        }
                    },
                ),
            ),
            None => failed(
                DoctorSection::Target,
                "local target configuration",
                "no local MySQL target is configured",
                "Run `reprodb setup`.",
                DoctorFailureKind::Configuration,
            ),
        });

        let source_password = if let Some((_, profile)) = &active {
            match credentials.get(&profile.credential_key).await {
                Ok(password) => {
                    report.checks.push(passed(
                        DoctorSection::Source,
                        "source credential",
                        "credential is available in the OS store",
                    ));
                    Some(password)
                }
                Err(error) => {
                    report.checks.push(credential_failure(
                        DoctorSection::Source,
                        "source credential",
                        error,
                        "Recreate the source profile with `reprodb profile add NAME`.",
                    ));
                    None
                }
            }
        } else {
            report.checks.push(skipped(
                DoctorSection::Source,
                "source credential",
                "skipped because no source profile is active",
            ));
            None
        };

        let target_password = if let Some(target) = &target {
            match credentials.get(&target.credential_key).await {
                Ok(password) => {
                    report.checks.push(passed(
                        DoctorSection::Target,
                        "target credential",
                        "credential is available in the OS store",
                    ));
                    Some(password)
                }
                Err(error) => {
                    report.checks.push(credential_failure(
                        DoctorSection::Target,
                        "target credential",
                        error,
                        "Run `reprodb setup` to save the target credential again.",
                    ));
                    None
                }
            }
        } else {
            report.checks.push(skipped(
                DoctorSection::Target,
                "target credential",
                "skipped because no local target is configured",
            ));
            None
        };

        let inventory = match docker.inspect().await {
            Ok(inventory) => {
                report.checks.push(passed(
                    DoctorSection::Docker,
                    "Docker",
                    format!("local context {} is available", inventory.context),
                ));
                Some(inventory)
            }
            Err(error) => {
                let (detail, action) = match error {
                    DoctorDockerError::Unavailable => (
                        "Docker or its current context is unavailable",
                        "Start Docker and verify `docker context show`.",
                    ),
                    DoctorDockerError::RemoteContext => (
                        "the current Docker context is remote",
                        "Select a local Unix-socket Docker context.",
                    ),
                    DoctorDockerError::InvalidMetadata => (
                        "Docker returned invalid context or container metadata",
                        "Run `docker context inspect` and `docker container ls`.",
                    ),
                };
                report.checks.push(failed(
                    DoctorSection::Docker,
                    "Docker",
                    detail,
                    action,
                    DoctorFailureKind::Docker,
                ));
                None
            }
        };

        let docker_context_matches = config.as_ref().is_some_and(|config| {
            inventory.as_ref().is_some_and(|inventory| {
                config
                    .client_runtime
                    .docker_context
                    .as_ref()
                    .is_none_or(|configured| configured == &inventory.context)
            })
        });
        if config.is_some() && inventory.is_some() && !docker_context_matches {
            report.checks.push(failed(
                DoctorSection::Docker,
                "Docker context identity",
                "the current context differs from the configured context",
                "Select the configured local context or rerun setup and profile configuration.",
                DoctorFailureKind::Docker,
            ));
        }

        let target_identity_valid = match (&target, &inventory) {
            (Some(target), Some(inventory)) if docker_context_matches => {
                let exact = inventory.containers.iter().find(|container| {
                    container.id == target.container_id && container.name == target.container_name
                });
                match exact {
                    Some(container) if container.running => {
                        report.checks.push(passed(
                            DoctorSection::Target,
                            "target container identity",
                            "configured name and full container ID match a running container",
                        ));
                        true
                    }
                    Some(_) => {
                        report.checks.push(failed(
                            DoctorSection::Target,
                            "target container identity",
                            "the configured target container is stopped",
                            "Start the container or run `reprodb setup`.",
                            DoctorFailureKind::Docker,
                        ));
                        false
                    }
                    None => {
                        report.checks.push(failed(
                            DoctorSection::Target,
                            "target container identity",
                            "the configured container name and full ID no longer match",
                            "Run `reprodb setup` before any restore.",
                            DoctorFailureKind::Docker,
                        ));
                        false
                    }
                }
            }
            (Some(_), _) => {
                report.checks.push(skipped(
                    DoctorSection::Target,
                    "target container identity",
                    "skipped because Docker context validation failed",
                ));
                false
            }
            (None, _) => false,
        };

        self.check_source_connection(
            &mut report,
            active,
            source_password,
            docker_context_matches,
            source_verifier,
        )
        .await;
        self.check_target_connection(
            &mut report,
            target,
            target_password,
            target_identity_valid,
            target_verifier,
        )
        .await;

        report
    }

    async fn check_source_connection(
        &self,
        report: &mut DoctorReport,
        active: Option<(crate::domain::ProfileName, SourceProfileConfig)>,
        password: Option<secrecy::SecretString>,
        docker_ready: bool,
        verifier: &dyn SourceProfileVerifier,
    ) {
        let (Some((name, profile)), Some(password)) = (active, password) else {
            report.checks.push(skipped(
                DoctorSection::Source,
                "source connection",
                "skipped because profile or credential validation failed",
            ));
            return;
        };
        if !docker_ready {
            report.checks.push(skipped(
                DoctorSection::Source,
                "source connection",
                "skipped because Docker validation failed",
            ));
            return;
        }

        let configured_series = profile.mysql_series.clone();
        let tls_mode = profile.tls_mode;
        let production = profile.production;
        let central_database = match profile.tenant_resolver {
            crate::infrastructure::config::TenantResolverConfig::SaltCentral {
                central_database,
                ..
            } => central_database,
            crate::infrastructure::config::TenantResolverConfig::Pattern { .. } => {
                crate::domain::DatabaseName::try_from("salt_central")
                    .expect("the fallback central database name is valid")
            }
        };
        let input = NewProfileInput {
            name,
            host: profile.host,
            port: profile.port,
            username: profile.username,
            password,
            central_database,
            tls_mode: profile.tls_mode,
            tls_material: profile.tls_material,
            production: profile.production,
        };
        match verifier.verify(&input).await {
            Ok(verified) => {
                let detected_series = format!(
                    "{}.{}",
                    verified.server_version.major, verified.server_version.minor
                );
                if detected_series == configured_series {
                    let detail = connection_detail(
                        &verified.vendor,
                        verified.server_version,
                        verified.client.version(),
                        verified.tls_cipher.as_deref(),
                    );
                    if verified.tls_cipher.is_some() {
                        report.checks.push(passed(
                            DoctorSection::Source,
                            "source connection",
                            detail,
                        ));
                    } else if production {
                        report.checks.push(failed(
                            DoctorSection::Source,
                            "source connection",
                            detail,
                            "Require TLS and review the source server TLS configuration.",
                            DoctorFailureKind::SourceConnection,
                        ));
                    } else {
                        report.checks.push(warning(
                            DoctorSection::Source,
                            "source connection",
                            format!("{detail} · configured TLS {tls_mode}"),
                            "Use TLS REQUIRED when the source supports encrypted connections.",
                        ));
                    }
                } else {
                    report.checks.push(failed(
                        DoctorSection::Source,
                        "source connection",
                        "the source server series changed after profile creation",
                        "Recreate the profile after reviewing MySQL compatibility.",
                        DoctorFailureKind::Dependency,
                    ));
                }
            }
            Err(error) => report.checks.push(source_connection_failure(error)),
        }
    }

    async fn check_target_connection(
        &self,
        report: &mut DoctorReport,
        target: Option<crate::infrastructure::config::LocalTargetConfig>,
        password: Option<secrecy::SecretString>,
        identity_valid: bool,
        verifier: &dyn LocalTargetVerifier,
    ) {
        let (Some(target), Some(password)) = (target, password) else {
            report.checks.push(skipped(
                DoctorSection::Target,
                "target connection",
                "skipped because target or credential validation failed",
            ));
            return;
        };
        if !identity_valid {
            report.checks.push(skipped(
                DoctorSection::Target,
                "target connection",
                "skipped because container identity validation failed",
            ));
            return;
        }

        let input = NewLocalTargetInput {
            docker_context: target.docker_context,
            container_name: target.container_name,
            container_id: target.container_id,
            username: target.username,
            password,
            central_database: target.central_database,
            tenant_database_prefix: target.tenant_database_prefix,
            managed_by_reprodb: matches!(
                target.trust,
                crate::infrastructure::config::LocalTargetTrust::ReprodbManaged
            ),
        };
        match verifier.verify(&input).await {
            Ok(verified) => {
                let detected_series = format!(
                    "{}.{}",
                    verified.server_version.major, verified.server_version.minor
                );
                if detected_series != verified.client.series() {
                    report.checks.push(failed(
                        DoctorSection::Target,
                        "target connection",
                        "the target server series is incompatible with the approved client",
                        "Run setup with a supported local MySQL target.",
                        DoctorFailureKind::Dependency,
                    ));
                } else if verified.tls_cipher.is_none() {
                    report.checks.push(failed(
                        DoctorSection::Target,
                        "target connection",
                        connection_detail(
                            &verified.vendor,
                            verified.server_version,
                            verified.client.version(),
                            None,
                        ),
                        "Review the target MySQL TLS support, then rerun setup.",
                        DoctorFailureKind::Docker,
                    ));
                } else {
                    report.checks.push(passed(
                        DoctorSection::Target,
                        "target connection",
                        connection_detail(
                            &verified.vendor,
                            verified.server_version,
                            verified.client.version(),
                            verified.tls_cipher.as_deref(),
                        ),
                    ));
                }
            }
            Err(error) => report.checks.push(target_connection_failure(error)),
        }
    }
}

fn passed(section: DoctorSection, label: &'static str, detail: impl Into<String>) -> DoctorCheck {
    DoctorCheck {
        section,
        label,
        status: DoctorStatus::Passed,
        detail: detail.into(),
        action: None,
        failure_kind: None,
    }
}

fn warning(
    section: DoctorSection,
    label: &'static str,
    detail: impl Into<String>,
    action: &'static str,
) -> DoctorCheck {
    DoctorCheck {
        section,
        label,
        status: DoctorStatus::Warning,
        detail: detail.into(),
        action: Some(action),
        failure_kind: None,
    }
}

fn failed(
    section: DoctorSection,
    label: &'static str,
    detail: impl Into<String>,
    action: &'static str,
    failure_kind: DoctorFailureKind,
) -> DoctorCheck {
    DoctorCheck {
        section,
        label,
        status: DoctorStatus::Failed,
        detail: detail.into(),
        action: Some(action),
        failure_kind: Some(failure_kind),
    }
}

fn skipped(section: DoctorSection, label: &'static str, detail: impl Into<String>) -> DoctorCheck {
    DoctorCheck {
        section,
        label,
        status: DoctorStatus::Skipped,
        detail: detail.into(),
        action: None,
        failure_kind: None,
    }
}

fn credential_failure(
    section: DoctorSection,
    label: &'static str,
    error: CredentialError,
    action: &'static str,
) -> DoctorCheck {
    let detail = match error {
        CredentialError::NotFound => "credential is missing from the OS store",
        CredentialError::StoreUnavailable { .. }
        | CredentialError::OperationFailed { .. }
        | CredentialError::BackgroundTaskFailed { .. } => {
            "the OS credential store could not be accessed"
        }
    };
    failed(
        section,
        label,
        detail,
        action,
        DoctorFailureKind::Credential,
    )
}

fn connection_detail(
    vendor: &str,
    version: crate::domain::MysqlVersion,
    client_version: crate::domain::MysqlVersion,
    tls_cipher: Option<&str>,
) -> String {
    match tls_cipher {
        Some(cipher) => {
            format!(
                "server {vendor} {version} · client {client_version} · TLS encrypted ({cipher})"
            )
        }
        None => format!("server {vendor} {version} · client {client_version} · TLS not active"),
    }
}

fn source_connection_failure(error: SourceVerificationError) -> DoctorCheck {
    match error {
        SourceVerificationError::DockerUnavailable => failed(
            DoctorSection::Source,
            "source connection",
            "Docker became unavailable during the source check",
            "Start Docker and rerun doctor.",
            DoctorFailureKind::Docker,
        ),
        SourceVerificationError::ClientUnavailable
        | SourceVerificationError::UnsupportedServerSeries { .. } => failed(
            DoctorSection::Source,
            "source connection",
            "the approved MySQL client is absent or incompatible",
            "Run `reprodb profile add NAME` while online to prepare a supported client.",
            DoctorFailureKind::Dependency,
        ),
        SourceVerificationError::AuthenticationFailed => failed(
            DoctorSection::Source,
            "source connection",
            "the source rejected the stored credential",
            "Recreate the source profile with `reprodb profile add NAME`.",
            DoctorFailureKind::SourceConnection,
        ),
        SourceVerificationError::NetworkUnavailable => failed(
            DoctorSection::Source,
            "source connection",
            "the source could not be reached",
            "Review host, port, VPN and Docker networking, then rerun doctor.",
            DoctorFailureKind::SourceConnection,
        ),
        SourceVerificationError::TlsRequiredButNotNegotiated => failed(
            DoctorSection::Source,
            "source TLS",
            "the configured TLS policy was not satisfied",
            "Review the TLS mode, CA and source hostname, then rerun doctor.",
            DoctorFailureKind::SourceConnection,
        ),
        SourceVerificationError::InvalidMetadata => failed(
            DoctorSection::Source,
            "source connection",
            "the source returned invalid version or TLS metadata",
            "Review MySQL compatibility and rerun doctor.",
            DoctorFailureKind::SourceConnection,
        ),
    }
}

fn target_connection_failure(error: TargetVerificationError) -> DoctorCheck {
    match error {
        TargetVerificationError::ClientUnavailable
        | TargetVerificationError::UnsupportedServerSeries { .. } => failed(
            DoctorSection::Target,
            "target connection",
            "the approved MySQL client is absent or incompatible",
            "Run `reprodb setup` while online to prepare a supported client.",
            DoctorFailureKind::Dependency,
        ),
        TargetVerificationError::AuthenticationFailed => failed(
            DoctorSection::Target,
            "target connection",
            "the target rejected the stored credential",
            "Run `reprodb setup` to save the target credential again.",
            DoctorFailureKind::Credential,
        ),
        TargetVerificationError::ContainerNotRunning
        | TargetVerificationError::ConnectionUnavailable => failed(
            DoctorSection::Target,
            "target connection",
            "the selected local MySQL could not be reached",
            "Review container health, then rerun doctor.",
            DoctorFailureKind::Docker,
        ),
        TargetVerificationError::InvalidMetadata => failed(
            DoctorSection::Target,
            "target connection",
            "the target returned invalid version or TLS metadata",
            "Review the target MySQL compatibility, then rerun setup.",
            DoctorFailureKind::Docker,
        ),
    }
}

fn format_gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1024_f64.powi(3)))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use secrecy::SecretString;
    use tempfile::TempDir;

    use super::*;
    use crate::{
        application::{TargetVerificationError, VerifiedLocalTarget, VerifiedSource},
        domain::{CredentialKey, CredentialScope, DatabaseName, MysqlTlsMode, ProfileName},
        infrastructure::{
            config::{
                AppConfig, AppPaths, ClientRuntimeConfig, LocalTargetConfig, MysqlClientConfig,
                MysqlFamily, SourceProfileConfig, TenantResolverConfig,
            },
            credentials::MemoryCredentialStore,
            mysql::ClientCatalog,
        },
    };

    fn repository(temp: &TempDir) -> ConfigRepository {
        ConfigRepository::new(AppPaths::new(
            temp.path().join("config"),
            temp.path().join("cache"),
            temp.path().join("data"),
        ))
    }

    fn valid_config() -> (AppConfig, CredentialKey, CredentialKey) {
        let client = ClientCatalog::resolve("8.4").unwrap();
        let profile_name = ProfileName::try_from("local-source").unwrap();
        let source_key = CredentialKey::new(CredentialScope::Source);
        let target_key = CredentialKey::new(CredentialScope::Target);
        let container_id = ContainerId::try_from("a".repeat(64)).unwrap();
        let config = AppConfig {
            active_profile: Some(profile_name.clone()),
            client_runtime: ClientRuntimeConfig {
                docker_context: Some("desktop-linux".to_owned()),
                ..Default::default()
            },
            local_target: Some(LocalTargetConfig {
                docker_context: "desktop-linux".to_owned(),
                container_name: ContainerName::try_from("mysql-8").unwrap(),
                container_id,
                username: "root".to_owned(),
                credential_key: target_key,
                central_database: DatabaseName::try_from("salt_central").unwrap(),
                trust: crate::infrastructure::config::LocalTargetTrust::UserConfirmed,
                tenant_database_prefix: "salt_".to_owned(),
            }),
            profiles: BTreeMap::from([(
                profile_name,
                SourceProfileConfig {
                    host: "127.0.0.1".to_owned(),
                    port: 3306,
                    username: "root".to_owned(),
                    credential_key: source_key,
                    mysql_family: MysqlFamily::Mysql,
                    mysql_series: "8.4".to_owned(),
                    production: false,
                    tls_mode: MysqlTlsMode::Required,
                    tls_material: Default::default(),
                    client: MysqlClientConfig {
                        image: client.image().to_owned(),
                    },
                    tenant_resolver: TenantResolverConfig::SaltCentral {
                        central_database: DatabaseName::try_from("salt_central").unwrap(),
                        allow_domain_lookup: true,
                    },
                },
            )]),
            ..Default::default()
        };
        (config, source_key, target_key)
    }

    struct FakeDocker {
        result: Result<DoctorDockerInventory, DoctorDockerError>,
    }

    struct FakeStorage {
        result: Result<u64, DoctorStorageError>,
    }

    impl DoctorStorageInspector for FakeStorage {
        fn available_bytes(&self, _path: &Path) -> Result<u64, DoctorStorageError> {
            self.result
        }
    }

    fn sufficient_storage() -> FakeStorage {
        FakeStorage {
            result: Ok(20 * 1024 * 1024 * 1024),
        }
    }

    #[async_trait]
    impl DoctorDockerInspector for FakeDocker {
        async fn inspect(&self) -> Result<DoctorDockerInventory, DoctorDockerError> {
            self.result.clone()
        }
    }

    struct FakeSourceVerifier {
        calls: AtomicUsize,
        result: Result<VerifiedSource, super::super::SourceVerificationError>,
    }

    #[async_trait]
    impl SourceProfileVerifier for FakeSourceVerifier {
        async fn verify(
            &self,
            _input: &NewProfileInput,
        ) -> Result<VerifiedSource, super::super::SourceVerificationError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result.clone()
        }
    }

    struct FakeTargetVerifier {
        calls: AtomicUsize,
        result: Result<VerifiedLocalTarget, TargetVerificationError>,
    }

    #[async_trait]
    impl LocalTargetVerifier for FakeTargetVerifier {
        async fn verify(
            &self,
            _input: &NewLocalTargetInput,
        ) -> Result<VerifiedLocalTarget, TargetVerificationError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result.clone()
        }
    }

    fn successful_source() -> FakeSourceVerifier {
        FakeSourceVerifier {
            calls: AtomicUsize::new(0),
            result: Ok(VerifiedSource {
                docker_context: "desktop-linux".to_owned(),
                server_version: "8.4.4".parse().unwrap(),
                vendor: "MySQL Community Server".to_owned(),
                tls_cipher: Some("TLS_AES_256_GCM_SHA384".to_owned()),
                client: ClientCatalog::resolve("8.4").unwrap(),
            }),
        }
    }

    fn successful_target() -> FakeTargetVerifier {
        FakeTargetVerifier {
            calls: AtomicUsize::new(0),
            result: Ok(VerifiedLocalTarget {
                server_version: "8.4.4".parse().unwrap(),
                server_uuid: "22222222-2222-4222-8222-222222222222".parse().unwrap(),
                vendor: "MySQL Community Server".to_owned(),
                tls_cipher: Some("TLS_AES_256_GCM_SHA384".to_owned()),
                client: ClientCatalog::resolve("8.4").unwrap(),
            }),
        }
    }

    fn docker_inventory(container_id: ContainerId) -> FakeDocker {
        FakeDocker {
            result: Ok(DoctorDockerInventory {
                context: "desktop-linux".to_owned(),
                containers: vec![DoctorDockerContainer {
                    id: container_id,
                    name: ContainerName::try_from("mysql-8").unwrap(),
                    running: true,
                }],
            }),
        }
    }

    #[tokio::test]
    async fn complete_environment_reports_ready_without_exposing_credentials() {
        let temp = TempDir::new().unwrap();
        let repository = repository(&temp);
        let (config, source_key, target_key) = valid_config();
        let container_id = config.local_target.as_ref().unwrap().container_id.clone();
        repository.save(&config).unwrap();
        let credentials = MemoryCredentialStore::default();
        credentials
            .set(&source_key, SecretString::from("source-password-marker"))
            .await
            .unwrap();
        credentials
            .set(&target_key, SecretString::from("target-password-marker"))
            .await
            .unwrap();
        let source = successful_source();
        let target = successful_target();

        let report = DoctorService::new(repository)
            .run(
                &credentials,
                &docker_inventory(container_id),
                &sufficient_storage(),
                &source,
                &target,
            )
            .await;

        assert!(report.is_ready());
        assert_eq!(source.calls.load(Ordering::SeqCst), 1);
        assert_eq!(target.calls.load(Ordering::SeqCst), 1);
        let rendered = format!("{report:?}");
        assert!(!rendered.contains("source-password-marker"));
        assert!(!rendered.contains("target-password-marker"));
        assert!(report.checks.iter().any(|check| {
            check.label == "target container identity" && check.status == DoctorStatus::Passed
        }));
    }

    #[tokio::test]
    async fn missing_configuration_reports_actions_and_skips_connections() {
        let temp = TempDir::new().unwrap();
        let source = successful_source();
        let target = successful_target();

        let report = DoctorService::new(repository(&temp))
            .run(
                &MemoryCredentialStore::default(),
                &docker_inventory(ContainerId::try_from("a".repeat(64)).unwrap()),
                &sufficient_storage(),
                &source,
                &target,
            )
            .await;

        assert_eq!(
            report.first_failure_kind(),
            Some(DoctorFailureKind::Configuration)
        );
        assert_eq!(source.calls.load(Ordering::SeqCst), 0);
        assert_eq!(target.calls.load(Ordering::SeqCst), 0);
        assert!(report.checks.iter().any(|check| {
            check.label == "configuration" && check.status == DoctorStatus::Warning
        }));
        assert!(report.checks.iter().any(|check| {
            check.label == "source connection" && check.status == DoctorStatus::Skipped
        }));
    }

    #[tokio::test]
    async fn changed_container_identity_blocks_only_the_target_connection() {
        let temp = TempDir::new().unwrap();
        let repository = repository(&temp);
        let (config, source_key, target_key) = valid_config();
        repository.save(&config).unwrap();
        let credentials = MemoryCredentialStore::default();
        credentials
            .set(&source_key, SecretString::from("source-secret"))
            .await
            .unwrap();
        credentials
            .set(&target_key, SecretString::from("target-secret"))
            .await
            .unwrap();
        let source = successful_source();
        let target = successful_target();

        let report = DoctorService::new(repository)
            .run(
                &credentials,
                &docker_inventory(ContainerId::try_from("b".repeat(64)).unwrap()),
                &sufficient_storage(),
                &source,
                &target,
            )
            .await;

        assert_eq!(source.calls.load(Ordering::SeqCst), 1);
        assert_eq!(target.calls.load(Ordering::SeqCst), 0);
        assert!(report.checks.iter().any(|check| {
            check.label == "target container identity" && check.status == DoctorStatus::Failed
        }));
    }

    #[tokio::test]
    async fn insufficient_storage_fails_without_preventing_independent_checks() {
        let temp = TempDir::new().unwrap();
        let repository = repository(&temp);
        let (config, source_key, target_key) = valid_config();
        let container_id = config.local_target.as_ref().unwrap().container_id.clone();
        repository.save(&config).unwrap();
        let credentials = MemoryCredentialStore::default();
        credentials
            .set(&source_key, SecretString::from("source-secret"))
            .await
            .unwrap();
        credentials
            .set(&target_key, SecretString::from("target-secret"))
            .await
            .unwrap();
        let source = successful_source();
        let target = successful_target();

        let report = DoctorService::new(repository)
            .run(
                &credentials,
                &docker_inventory(container_id),
                &FakeStorage {
                    result: Ok(MINIMUM_FREE_BYTES - 1),
                },
                &source,
                &target,
            )
            .await;

        assert_eq!(
            report.first_failure_kind(),
            Some(DoctorFailureKind::Filesystem)
        );
        assert_eq!(source.calls.load(Ordering::SeqCst), 1);
        assert_eq!(target.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn absent_approved_client_is_reported_as_a_dependency() {
        let temp = TempDir::new().unwrap();
        let repository = repository(&temp);
        let (config, source_key, target_key) = valid_config();
        let container_id = config.local_target.as_ref().unwrap().container_id.clone();
        repository.save(&config).unwrap();
        let credentials = MemoryCredentialStore::default();
        credentials
            .set(&source_key, SecretString::from("source-secret"))
            .await
            .unwrap();
        credentials
            .set(&target_key, SecretString::from("target-secret"))
            .await
            .unwrap();
        let source = FakeSourceVerifier {
            calls: AtomicUsize::new(0),
            result: Err(SourceVerificationError::ClientUnavailable),
        };

        let report = DoctorService::new(repository)
            .run(
                &credentials,
                &docker_inventory(container_id),
                &sufficient_storage(),
                &source,
                &successful_target(),
            )
            .await;

        assert_eq!(
            report.first_failure_kind(),
            Some(DoctorFailureKind::Dependency)
        );
        assert!(report.checks.iter().any(|check| {
            check.label == "source connection"
                && check.detail.contains("approved MySQL client")
                && check
                    .action
                    .is_some_and(|action| action.contains("while online"))
        }));
    }

    #[tokio::test]
    async fn unencrypted_non_production_source_is_an_explicit_warning() {
        let temp = TempDir::new().unwrap();
        let repository = repository(&temp);
        let (mut config, source_key, target_key) = valid_config();
        let profile = config.profiles.values_mut().next().unwrap();
        profile.tls_mode = MysqlTlsMode::Preferred;
        let container_id = config.local_target.as_ref().unwrap().container_id.clone();
        repository.save(&config).unwrap();
        let credentials = MemoryCredentialStore::default();
        credentials
            .set(&source_key, SecretString::from("source-secret"))
            .await
            .unwrap();
        credentials
            .set(&target_key, SecretString::from("target-secret"))
            .await
            .unwrap();
        let source = FakeSourceVerifier {
            calls: AtomicUsize::new(0),
            result: Ok(VerifiedSource {
                tls_cipher: None,
                ..successful_source().result.unwrap()
            }),
        };

        let report = DoctorService::new(repository)
            .run(
                &credentials,
                &docker_inventory(container_id),
                &sufficient_storage(),
                &source,
                &successful_target(),
            )
            .await;

        assert!(report.is_ready());
        assert!(report.checks.iter().any(|check| {
            check.label == "source connection"
                && check.status == DoctorStatus::Warning
                && check.detail.contains("TLS not active")
        }));
    }
}
