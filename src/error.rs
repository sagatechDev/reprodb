use thiserror::Error;

use crate::domain::ValueObjectError;

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

    #[error("command `{command}` is not implemented yet")]
    CommandNotImplemented { command: &'static str },

    #[error("operation interrupted")]
    Interrupted,
}

impl AppError {
    pub const fn category(&self) -> ErrorCategory {
        match self {
            Self::InvalidValue(..) => ErrorCategory::Usage,
            Self::CommandNotImplemented { .. } => ErrorCategory::General,
            Self::Interrupted => ErrorCategory::Interrupted,
        }
    }

    pub const fn exit_code(&self) -> u8 {
        self.category().exit_code()
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
    fn not_implemented_error_contains_only_the_static_command_name() {
        let error = AppError::CommandNotImplemented { command: "pull" };

        assert_eq!(error.to_string(), "command `pull` is not implemented yet");
        assert_eq!(error.exit_code(), 1);
    }
}
