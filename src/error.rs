use thiserror::Error;

use crate::{
    application::{
        CacheServiceError, CredentialProvisionError, DoctorFailureKind, DumpServiceError,
        LocalTargetAttestationError, LocalTargetGateError, LocalTenantRegistrationServiceError,
        LocalTenantWriteError, ProfileServiceError, PullServiceError, RestoreEngineError,
        RestoreServiceError, SetupServiceError, SourceVerificationError, TargetVerificationError,
    },
    cli::prompt::PromptError,
    domain::{DumpMetadataError, TenantResolutionError, ValueObjectError},
    infrastructure::config::ConfigError,
    infrastructure::docker::DockerDiscoveryError,
    infrastructure::mysql::{
        DockerClientError, DumpExecutorError, DumpFailureKind, DumpPreflightError,
        RestoreExecutorError, RestoreFailureKind,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorCategory {
    General,
    Usage,
    Configuration,
    Credential,
    Dependency,
    SourceConnection,
    TenantResolution,
    Dump,
    Cache,
    Docker,
    Restore,
    Interrupted,
}

impl ErrorCategory {
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::General => 1,
            Self::Usage => 2,
            Self::Configuration => 10,
            Self::Credential => 11,
            Self::Dependency => 20,
            Self::SourceConnection => 30,
            Self::TenantResolution => 31,
            Self::Dump => 40,
            Self::Cache => 50,
            Self::Docker => 60,
            Self::Restore => 70,
            Self::Interrupted => 130,
        }
    }
}

#[derive(Debug, Error)]
pub enum AppError {
    #[error(transparent)]
    InvalidValue(#[from] ValueObjectError),

    #[error(transparent)]
    InvalidDumpId(#[from] DumpMetadataError),

    #[error(transparent)]
    TenantResolution(#[from] TenantResolutionError),

    #[error(transparent)]
    Profile(#[from] ProfileServiceError),

    #[error(transparent)]
    Configuration(#[from] ConfigError),

    #[error(transparent)]
    Prompt(#[from] PromptError),

    #[error(transparent)]
    DockerDiscovery(#[from] DockerDiscoveryError),

    #[error(transparent)]
    Setup(#[from] SetupServiceError),

    #[error(transparent)]
    Dump(#[from] DumpServiceError),

    #[error(transparent)]
    Restore(#[from] RestoreServiceError),

    #[error(transparent)]
    Pull(#[from] PullServiceError),

    #[error(transparent)]
    Cache(#[from] CacheServiceError),

    #[error("doctor found required problems; review the failed checks above")]
    DoctorChecksFailed { kind: DoctorFailureKind },

    #[error("could not write CLI output")]
    Output(#[source] std::io::Error),

    #[error("operation interrupted")]
    Interrupted,
}

impl AppError {
    pub const fn category(&self) -> ErrorCategory {
        match self {
            Self::InvalidValue(..) => ErrorCategory::Usage,
            Self::InvalidDumpId(..) => ErrorCategory::Usage,
            Self::TenantResolution(
                TenantResolutionError::InvalidPattern
                | TenantResolutionError::InvalidTenantId(_)
                | TenantResolutionError::InvalidDatabase(_)
                | TenantResolutionError::NotFound
                | TenantResolutionError::Ambiguous
                | TenantResolutionError::InvalidMetadata
                | TenantResolutionError::ConnectionOverride,
            ) => ErrorCategory::TenantResolution,
            Self::TenantResolution(
                TenantResolutionError::SourceUnavailable
                | TenantResolutionError::AuthenticationFailed,
            ) => ErrorCategory::SourceConnection,
            Self::TenantResolution(TenantResolutionError::ClientUnavailable) => {
                ErrorCategory::Dependency
            }
            Self::Profile(ProfileServiceError::Config(_))
            | Self::Profile(ProfileServiceError::NotFound)
            | Self::Profile(ProfileServiceError::AlreadyExists) => ErrorCategory::Configuration,
            Self::Profile(ProfileServiceError::InvalidField { .. }) => ErrorCategory::Usage,
            Self::Profile(ProfileServiceError::Verification(
                SourceVerificationError::DockerUnavailable,
            )) => ErrorCategory::Docker,
            Self::Profile(ProfileServiceError::Verification(
                SourceVerificationError::ClientUnavailable
                | SourceVerificationError::UnsupportedServerSeries,
            )) => ErrorCategory::Dependency,
            Self::Profile(ProfileServiceError::Verification(
                SourceVerificationError::NetworkUnavailable
                | SourceVerificationError::AuthenticationFailed
                | SourceVerificationError::InvalidMetadata,
            )) => ErrorCategory::SourceConnection,
            Self::Profile(ProfileServiceError::Provision(
                CredentialProvisionError::Credential(_)
                | CredentialProvisionError::CredentialAlreadyExists
                | CredentialProvisionError::ConfigAndRollback { .. },
            )) => ErrorCategory::Credential,
            Self::Profile(ProfileServiceError::Provision(
                CredentialProvisionError::InvalidCredentialReference
                | CredentialProvisionError::Config { .. },
            )) => ErrorCategory::Configuration,
            Self::Profile(ProfileServiceError::CredentialCleanup { .. }) => {
                ErrorCategory::Credential
            }
            Self::Configuration(_) => ErrorCategory::Configuration,
            Self::Prompt(_) => ErrorCategory::Usage,
            Self::DockerDiscovery(_) => ErrorCategory::Docker,
            Self::Setup(SetupServiceError::Config(_)) => ErrorCategory::Configuration,
            Self::Setup(SetupServiceError::NoCandidates) => ErrorCategory::Docker,
            Self::Setup(SetupServiceError::InvalidField { .. }) => ErrorCategory::Usage,
            Self::Setup(SetupServiceError::Verification(
                TargetVerificationError::ClientUnavailable
                | TargetVerificationError::UnsupportedServerSeries,
            )) => ErrorCategory::Dependency,
            Self::Setup(SetupServiceError::Verification(
                TargetVerificationError::AuthenticationFailed,
            )) => ErrorCategory::Credential,
            Self::Setup(SetupServiceError::Verification(
                TargetVerificationError::ContainerNotRunning
                | TargetVerificationError::ConnectionUnavailable
                | TargetVerificationError::InvalidMetadata,
            )) => ErrorCategory::Docker,
            Self::Setup(SetupServiceError::Provision(
                CredentialProvisionError::Credential(_)
                | CredentialProvisionError::CredentialAlreadyExists
                | CredentialProvisionError::ConfigAndRollback { .. },
            ))
            | Self::Setup(SetupServiceError::PreviousCredentialCleanup { .. }) => {
                ErrorCategory::Credential
            }
            Self::Setup(SetupServiceError::Provision(
                CredentialProvisionError::InvalidCredentialReference
                | CredentialProvisionError::Config { .. },
            )) => ErrorCategory::Configuration,
            Self::Dump(error) => dump_error_category(error),
            Self::Restore(error) => restore_error_category(error),
            Self::Pull(error) => pull_error_category(error),
            Self::Cache(error) => cache_error_category(error),
            Self::DoctorChecksFailed { kind } => match kind {
                DoctorFailureKind::Configuration => ErrorCategory::Configuration,
                DoctorFailureKind::Credential => ErrorCategory::Credential,
                DoctorFailureKind::Dependency => ErrorCategory::Dependency,
                DoctorFailureKind::SourceConnection => ErrorCategory::SourceConnection,
                DoctorFailureKind::Docker => ErrorCategory::Docker,
                DoctorFailureKind::Filesystem => ErrorCategory::Cache,
            },
            Self::Output(_) => ErrorCategory::General,
            Self::Interrupted => ErrorCategory::Interrupted,
        }
    }

    pub const fn exit_code(&self) -> u8 {
        self.category().exit_code()
    }

    pub const fn should_render_on_stderr(&self) -> bool {
        !matches!(self, Self::DoctorChecksFailed { .. })
    }
}

const fn cache_error_category(error: &CacheServiceError) -> ErrorCategory {
    match error {
        CacheServiceError::Config(_) | CacheServiceError::NoActiveProfile => {
            ErrorCategory::Configuration
        }
        CacheServiceError::Clock(_) => ErrorCategory::General,
        CacheServiceError::Cache(_) | CacheServiceError::Cleanup(_) => ErrorCategory::Cache,
    }
}

const fn pull_error_category(error: &PullServiceError) -> ErrorCategory {
    match error {
        PullServiceError::Config(_) | PullServiceError::NoActiveProfile => {
            ErrorCategory::Configuration
        }
        PullServiceError::Clock(_) => ErrorCategory::General,
        PullServiceError::DatabaseSelection(_) | PullServiceError::TargetSelection(_) => {
            ErrorCategory::Usage
        }
        PullServiceError::Cache(_) => ErrorCategory::Cache,
        PullServiceError::Dump(error) => dump_error_category(error),
        PullServiceError::Restore(error) => restore_error_category(error),
    }
}

const fn restore_error_category(error: &RestoreServiceError) -> ErrorCategory {
    match error {
        RestoreServiceError::Artifact(_) => ErrorCategory::Cache,
        RestoreServiceError::RegistrationData(_) => ErrorCategory::Restore,
        RestoreServiceError::Target(error) => local_target_error_category(error),
        RestoreServiceError::Engine(RestoreEngineError::VersionMismatch) => {
            ErrorCategory::Dependency
        }
        RestoreServiceError::Engine(RestoreEngineError::Lock(_) | RestoreEngineError::State(_)) => {
            ErrorCategory::Cache
        }
        RestoreServiceError::Engine(RestoreEngineError::Execution(error)) => {
            restore_executor_error_category(error)
        }
        RestoreServiceError::Engine(RestoreEngineError::ImportedSizeMismatch) => {
            ErrorCategory::Restore
        }
        RestoreServiceError::Registration(error) => match error {
            LocalTenantRegistrationServiceError::RestoreIdentityMismatch => ErrorCategory::Restore,
            LocalTenantRegistrationServiceError::Write(
                LocalTenantWriteError::AuthenticationFailed,
            ) => ErrorCategory::Credential,
            LocalTenantRegistrationServiceError::Write(
                LocalTenantWriteError::TargetUnavailable,
            ) => ErrorCategory::Docker,
            LocalTenantRegistrationServiceError::Write(_) => ErrorCategory::Restore,
        },
    }
}

const fn local_target_error_category(error: &LocalTargetGateError) -> ErrorCategory {
    match error {
        LocalTargetGateError::Config(_)
        | LocalTargetGateError::NotConfigured
        | LocalTargetGateError::UnknownTarget
        | LocalTargetGateError::RuntimeContextMissing
        | LocalTargetGateError::DatabaseOutsideAllowlist => ErrorCategory::Configuration,
        LocalTargetGateError::Credential(_) => ErrorCategory::Credential,
        LocalTargetGateError::Attestation(LocalTargetAttestationError::ClientUnavailable)
        | LocalTargetGateError::UnsupportedVendor
        | LocalTargetGateError::UnsupportedServerSeries => ErrorCategory::Dependency,
        LocalTargetGateError::Attestation(LocalTargetAttestationError::AuthenticationFailed) => {
            ErrorCategory::Credential
        }
        LocalTargetGateError::Attestation(_)
        | LocalTargetGateError::ContextIdentityChanged
        | LocalTargetGateError::ContainerIdentityChanged
        | LocalTargetGateError::ManagedMarkerMissing => ErrorCategory::Docker,
    }
}

const fn restore_executor_error_category(error: &RestoreExecutorError) -> ErrorCategory {
    match error {
        RestoreExecutorError::OptionFile(_) => ErrorCategory::Credential,
        RestoreExecutorError::OpenArtifact(_)
        | RestoreExecutorError::DecodeArtifact(_)
        | RestoreExecutorError::ArtifactChangedDuringImport => ErrorCategory::Cache,
        RestoreExecutorError::RecreateFailed { kind, .. }
        | RestoreExecutorError::ImportFailed { kind, .. } => restore_failure_category(*kind),
        RestoreExecutorError::Start(_)
        | RestoreExecutorError::Wait(_)
        | RestoreExecutorError::MissingStdin
        | RestoreExecutorError::MissingStderr
        | RestoreExecutorError::ReadStderr(_)
        | RestoreExecutorError::StderrTask(_) => ErrorCategory::Docker,
        RestoreExecutorError::DecoderTask(_)
        | RestoreExecutorError::ArtifactTooLarge
        | RestoreExecutorError::MysqlStoppedEarly
        | RestoreExecutorError::WriteStdin(_)
        | RestoreExecutorError::CloseStdin(_) => ErrorCategory::Restore,
    }
}

const fn restore_failure_category(kind: RestoreFailureKind) -> ErrorCategory {
    match kind {
        RestoreFailureKind::Authentication => ErrorCategory::Credential,
        RestoreFailureKind::TargetUnavailable | RestoreFailureKind::DockerUnavailable => {
            ErrorCategory::Docker
        }
        RestoreFailureKind::Permission | RestoreFailureKind::Sql | RestoreFailureKind::Unknown => {
            ErrorCategory::Restore
        }
    }
}

const fn dump_error_category(error: &DumpServiceError) -> ErrorCategory {
    match error {
        DumpServiceError::Config(_)
        | DumpServiceError::NoActiveProfile
        | DumpServiceError::DockerContextMissing => ErrorCategory::Configuration,
        DumpServiceError::ProductionNotEnabled => ErrorCategory::Dump,
        DumpServiceError::ClientCatalog(_) => ErrorCategory::Dependency,
        DumpServiceError::Credential(_) => ErrorCategory::Credential,
        DumpServiceError::Tenant(error) => match error {
            TenantResolutionError::SourceUnavailable
            | TenantResolutionError::AuthenticationFailed => ErrorCategory::SourceConnection,
            TenantResolutionError::ClientUnavailable => ErrorCategory::Dependency,
            _ => ErrorCategory::TenantResolution,
        },
        DumpServiceError::Preflight(DumpPreflightError::Client(error))
        | DumpServiceError::Execute(DumpExecutorError::Client(error)) => {
            docker_client_error_category(error)
        }
        DumpServiceError::Execute(DumpExecutorError::Start(_)) => ErrorCategory::Docker,
        DumpServiceError::Execute(DumpExecutorError::ProcessFailed { kind, .. }) => match kind {
            DumpFailureKind::Authentication | DumpFailureKind::SourceUnavailable => {
                ErrorCategory::SourceConnection
            }
            DumpFailureKind::DockerUnavailable => ErrorCategory::Docker,
            DumpFailureKind::Permission
            | DumpFailureKind::DatabaseUnavailable
            | DumpFailureKind::Unknown => ErrorCategory::Dump,
        },
        DumpServiceError::Lock(_)
        | DumpServiceError::Artifact(_)
        | DumpServiceError::Cleanup(_) => ErrorCategory::Cache,
        DumpServiceError::Preflight(_)
        | DumpServiceError::Execute(_)
        | DumpServiceError::Metadata(_) => ErrorCategory::Dump,
        DumpServiceError::Clock(_) => ErrorCategory::General,
    }
}

const fn docker_client_error_category(error: &DockerClientError) -> ErrorCategory {
    match error {
        DockerClientError::Catalog(_)
        | DockerClientError::ImagePullFailed
        | DockerClientError::ImageUnavailable
        | DockerClientError::ImageInspectFailed
        | DockerClientError::InvalidImageMetadata
        | DockerClientError::ImageDigestMismatch
        | DockerClientError::VersionProbeFailed
        | DockerClientError::IncompatibleClientVersion => ErrorCategory::Dependency,
        DockerClientError::DockerUnavailable
        | DockerClientError::InvalidDockerContext
        | DockerClientError::Process(_) => ErrorCategory::Docker,
        DockerClientError::SourceNetworkUnavailable
        | DockerClientError::AuthenticationFailed
        | DockerClientError::ConnectionProbeFailed
        | DockerClientError::QueryFailed
        | DockerClientError::InvalidServerMetadata => ErrorCategory::SourceConnection,
        DockerClientError::OptionFile(_) | DockerClientError::OptionFilePathNotAbsolute => {
            ErrorCategory::Credential
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn category_exit_codes_are_unique_and_stable() {
        let categories = [
            ErrorCategory::General,
            ErrorCategory::Usage,
            ErrorCategory::Configuration,
            ErrorCategory::Credential,
            ErrorCategory::Dependency,
            ErrorCategory::SourceConnection,
            ErrorCategory::TenantResolution,
            ErrorCategory::Dump,
            ErrorCategory::Cache,
            ErrorCategory::Docker,
            ErrorCategory::Restore,
            ErrorCategory::Interrupted,
        ];
        let codes: HashSet<_> = categories
            .map(ErrorCategory::exit_code)
            .into_iter()
            .collect();

        assert_eq!(codes.len(), categories.len());
        assert_eq!(ErrorCategory::Usage.exit_code(), 2);
        assert_eq!(ErrorCategory::Interrupted.exit_code(), 130);
    }

    #[test]
    fn failed_credential_cleanup_uses_the_credential_exit_code() {
        let error = AppError::Profile(ProfileServiceError::CredentialCleanup {
            orphaned_key: crate::domain::CredentialKey::new(crate::domain::CredentialScope::Source),
            source: crate::infrastructure::credentials::CredentialError::StoreUnavailable {
                operation: crate::infrastructure::credentials::CredentialOperation::Delete,
            },
        });

        assert_eq!(error.exit_code(), 11);
        assert!(!error.to_string().contains("password"));
    }

    #[test]
    fn cache_commands_keep_configuration_and_filesystem_exit_categories_distinct() {
        assert_eq!(
            AppError::Cache(CacheServiceError::NoActiveProfile).exit_code(),
            10
        );
        assert_eq!(
            AppError::Cache(CacheServiceError::Cleanup(
                crate::infrastructure::cache_cleanup::CacheCleanupError::InvalidManagedPath
            ))
            .exit_code(),
            50
        );
    }

    #[test]
    fn source_verification_errors_keep_actionable_exit_categories() {
        let authentication = AppError::Profile(ProfileServiceError::Verification(
            SourceVerificationError::AuthenticationFailed,
        ));
        let docker = AppError::Profile(ProfileServiceError::Verification(
            SourceVerificationError::DockerUnavailable,
        ));
        let client = AppError::Profile(ProfileServiceError::Verification(
            SourceVerificationError::ClientUnavailable,
        ));

        assert_eq!(authentication.exit_code(), 30);
        assert_eq!(docker.exit_code(), 60);
        assert_eq!(client.exit_code(), 20);
    }

    #[test]
    fn doctor_failure_is_not_repeated_after_its_structured_report() {
        let error = AppError::DoctorChecksFailed {
            kind: DoctorFailureKind::Docker,
        };

        assert!(!error.should_render_on_stderr());
        assert_eq!(error.exit_code(), 60);
    }

    #[test]
    fn tenant_resolution_preserves_actionable_failure_categories() {
        assert_eq!(
            AppError::from(TenantResolutionError::NotFound).exit_code(),
            31
        );
        assert_eq!(
            AppError::from(TenantResolutionError::SourceUnavailable).exit_code(),
            30
        );
        assert_eq!(
            AppError::from(TenantResolutionError::ClientUnavailable).exit_code(),
            20
        );
    }

    #[test]
    fn dump_process_failures_keep_actionable_exit_categories() {
        let failure = |kind| {
            AppError::from(DumpServiceError::Execute(
                DumpExecutorError::ProcessFailed {
                    exit_code: Some(2),
                    kind,
                    stderr_truncated: false,
                },
            ))
        };

        assert_eq!(failure(DumpFailureKind::Authentication).exit_code(), 30);
        assert_eq!(failure(DumpFailureKind::DockerUnavailable).exit_code(), 60);
        assert_eq!(failure(DumpFailureKind::Unknown).exit_code(), 40);
    }

    #[test]
    fn restore_failures_keep_cache_configuration_and_credential_categories() {
        let missing = AppError::from(RestoreServiceError::Artifact(
            crate::infrastructure::restore_artifact::RestoreArtifactError::NotFound,
        ));
        let target = AppError::from(RestoreServiceError::Target(
            LocalTargetGateError::NotConfigured,
        ));
        let registration = AppError::from(RestoreServiceError::Registration(
            LocalTenantRegistrationServiceError::Write(LocalTenantWriteError::AuthenticationFailed),
        ));

        assert_eq!(missing.exit_code(), 50);
        assert_eq!(target.exit_code(), 10);
        assert_eq!(registration.exit_code(), 11);
    }

    #[test]
    fn pull_preserves_the_category_of_its_failed_phase() {
        let configuration = AppError::from(PullServiceError::NoActiveProfile);
        let cache = AppError::from(PullServiceError::Restore(RestoreServiceError::Artifact(
            crate::infrastructure::restore_artifact::RestoreArtifactError::NotFound,
        )));
        let source = AppError::from(PullServiceError::Dump(DumpServiceError::Tenant(
            TenantResolutionError::SourceUnavailable,
        )));

        assert_eq!(configuration.exit_code(), 10);
        assert_eq!(cache.exit_code(), 50);
        assert_eq!(source.exit_code(), 30);
    }
}
