use async_trait::async_trait;
use thiserror::Error;

use super::{DatabaseName, LocalTenantFeatures, TenantId, TenantLookup, ValueObjectError};

const TENANT_PLACEHOLDER: &str = "{tenant}";
const MAX_PATTERN_CHARACTERS: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TenantMatch {
    Database,
    Pattern,
    TenantId,
    Domain,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedTenant {
    pub tenant_id: TenantId,
    pub database: DatabaseName,
    pub matched_by: TenantMatch,
    pub features: LocalTenantFeatures,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum TenantResolutionError {
    #[error(
        "tenant pattern must contain exactly one `{{tenant}}` placeholder and only database-safe literals"
    )]
    InvalidPattern,

    #[error("resolved tenant ID is invalid")]
    InvalidTenantId(#[source] ValueObjectError),

    #[error("resolved database name is invalid or reserved")]
    InvalidDatabase(#[source] ValueObjectError),

    #[error("no tenant matched the supplied ID or domain")]
    NotFound,

    #[error("the supplied ID or domain matched more than one tenant")]
    Ambiguous,

    #[error("the tenant catalog returned invalid or unsupported metadata")]
    InvalidMetadata,

    #[error("the tenant catalog source could not be reached")]
    SourceUnavailable,

    #[error("the tenant catalog source rejected the configured credential")]
    AuthenticationFailed,

    #[error("the approved MySQL client is unavailable")]
    ClientUnavailable,

    #[error(
        "the tenant requires connection overrides that are incompatible with this source profile"
    )]
    ConnectionOverride,
}

#[async_trait]
pub trait TenantResolver: Send + Sync {
    async fn resolve(&self, lookup: &TenantLookup)
    -> Result<ResolvedTenant, TenantResolutionError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PatternTenantResolver {
    prefix: String,
    suffix: String,
}

impl PatternTenantResolver {
    pub fn new(pattern: &str) -> Result<Self, TenantResolutionError> {
        if pattern.chars().count() > MAX_PATTERN_CHARACTERS
            || pattern.matches(TENANT_PLACEHOLDER).count() != 1
        {
            return Err(TenantResolutionError::InvalidPattern);
        }

        let (prefix, suffix) = pattern
            .split_once(TENANT_PLACEHOLDER)
            .ok_or(TenantResolutionError::InvalidPattern)?;
        if !prefix
            .chars()
            .chain(suffix.chars())
            .all(is_database_literal)
        {
            return Err(TenantResolutionError::InvalidPattern);
        }

        Ok(Self {
            prefix: prefix.to_owned(),
            suffix: suffix.to_owned(),
        })
    }
}

#[async_trait]
impl TenantResolver for PatternTenantResolver {
    async fn resolve(
        &self,
        lookup: &TenantLookup,
    ) -> Result<ResolvedTenant, TenantResolutionError> {
        let tenant_id =
            TenantId::try_from(lookup.as_str()).map_err(TenantResolutionError::InvalidTenantId)?;
        let database =
            DatabaseName::try_from(format!("{}{}{}", self.prefix, lookup.as_str(), self.suffix))
                .map_err(TenantResolutionError::InvalidDatabase)?;

        Ok(ResolvedTenant {
            tenant_id,
            database,
            matched_by: TenantMatch::Pattern,
            features: LocalTenantFeatures::default(),
        })
    }
}

fn is_database_literal(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolves_a_valid_pattern_into_typed_values() {
        let resolver = PatternTenantResolver::new("salt_{tenant}").unwrap();
        let lookup = TenantLookup::try_from("sagatec").unwrap();

        let resolved = resolver.resolve(&lookup).await.unwrap();

        assert_eq!(resolved.tenant_id.as_str(), "sagatec");
        assert_eq!(resolved.database.as_str(), "salt_sagatec");
        assert_eq!(resolved.matched_by, TenantMatch::Pattern);
    }

    #[test]
    fn rejects_missing_repeated_or_disguised_placeholders() {
        for pattern in [
            "salt_tenant",
            "{tenant}_{tenant}",
            "salt_{{tenant}}",
            "salt_{Tenant}",
            "salt_{tenant}_*",
        ] {
            assert!(
                matches!(
                    PatternTenantResolver::new(pattern),
                    Err(TenantResolutionError::InvalidPattern)
                ),
                "pattern should have failed"
            );
        }
    }

    #[test]
    fn rejects_traversal_shell_sql_and_control_literals_at_construction() {
        for pattern in [
            "../{tenant}",
            "{tenant};DROP_DATABASE_mysql",
            "`{tenant}`",
            "{tenant}/database",
            "{tenant}\nnext",
        ] {
            assert!(PatternTenantResolver::new(pattern).is_err());
        }
    }

    #[tokio::test]
    async fn validates_the_final_database_length_and_reserved_names() {
        let oversized = PatternTenantResolver::new("prefix_{tenant}").unwrap();
        let long_lookup = TenantLookup::try_from("a".repeat(64)).unwrap();
        assert!(matches!(
            oversized.resolve(&long_lookup).await,
            Err(TenantResolutionError::InvalidDatabase(_))
        ));

        let identity = PatternTenantResolver::new("{tenant}").unwrap();
        let administrative = TenantLookup::try_from("mysql").unwrap();
        assert!(matches!(
            identity.resolve(&administrative).await,
            Err(TenantResolutionError::InvalidDatabase(_))
        ));
    }

    #[tokio::test]
    async fn a_hyphenated_lookup_cannot_bypass_database_validation() {
        let resolver = PatternTenantResolver::new("salt_{tenant}").unwrap();
        let lookup = TenantLookup::try_from("team-one").unwrap();

        assert!(matches!(
            resolver.resolve(&lookup).await,
            Err(TenantResolutionError::InvalidDatabase(_))
        ));
    }

    #[test]
    fn errors_never_repeat_the_untrusted_pattern() {
        let marker = "{tenant};password-marker";
        let error = PatternTenantResolver::new(marker).unwrap_err();

        assert!(!error.to_string().contains(marker));
        assert!(!format!("{error:?}").contains(marker));
    }
}
