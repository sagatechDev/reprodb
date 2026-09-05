use std::{fmt, str::FromStr};

use thiserror::Error;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValueKind {
    ProfileName,
    TenantLookup,
    TenantId,
    DomainAlias,
    DatabaseName,
    ContainerName,
    ContainerId,
    CredentialKey,
    MysqlVersion,
}

impl fmt::Display for ValueKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::ProfileName => "profile name",
            Self::TenantLookup => "tenant lookup",
            Self::TenantId => "tenant ID",
            Self::DomainAlias => "domain alias",
            Self::DatabaseName => "database name",
            Self::ContainerName => "container name",
            Self::ContainerId => "container ID",
            Self::CredentialKey => "credential key",
            Self::MysqlVersion => "MySQL version",
        };
        formatter.write_str(name)
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ValueObjectError {
    #[error("{kind} cannot be empty")]
    Empty { kind: ValueKind },

    #[error("{kind} exceeds the maximum length of {max} characters")]
    TooLong { kind: ValueKind, max: usize },

    #[error("{kind} has an invalid format; expected {expected}")]
    InvalidFormat {
        kind: ValueKind,
        expected: &'static str,
    },

    #[error("{kind} is reserved and cannot be used")]
    Reserved { kind: ValueKind },
}

macro_rules! validated_string {
    ($name:ident, $validator:ident) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = ValueObjectError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                $validator(value)?;
                Ok(Self(value.to_owned()))
            }
        }

        impl TryFrom<String> for $name {
            type Error = ValueObjectError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                $validator(&value)?;
                Ok(Self(value))
            }
        }

        impl TryFrom<&str> for $name {
            type Error = ValueObjectError;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                value.parse()
            }
        }
    };
}

validated_string!(ProfileName, validate_profile_name);
validated_string!(TenantLookup, validate_tenant_lookup);
validated_string!(TenantId, validate_tenant_id);
validated_string!(DomainAlias, validate_domain_alias);
validated_string!(DatabaseName, validate_database_name);
validated_string!(ContainerName, validate_container_name);
validated_string!(ContainerId, validate_container_id);

fn validate_profile_name(value: &str) -> Result<(), ValueObjectError> {
    validate_simple_identifier(
        value,
        ValueKind::ProfileName,
        64,
        "1-64 ASCII letters, digits, `_` or `-`, starting with a letter or digit",
        |byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'),
    )
}

fn validate_tenant_lookup(value: &str) -> Result<(), ValueObjectError> {
    validate_simple_identifier(
        value,
        ValueKind::TenantLookup,
        255,
        "1-255 ASCII letters, digits, `_` or `-`, starting with a letter or digit",
        |byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'),
    )
}

fn validate_tenant_id(value: &str) -> Result<(), ValueObjectError> {
    validate_simple_identifier(
        value,
        ValueKind::TenantId,
        255,
        "1-255 ASCII letters, digits, `_` or `-`, starting with a letter or digit",
        |byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'),
    )
}

fn validate_domain_alias(value: &str) -> Result<(), ValueObjectError> {
    const EXPECTED: &str =
        "a lowercase DNS label up to 63 characters, without leading or trailing `-`";
    validate_length(value, ValueKind::DomainAlias, 63)?;

    let valid = value
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && value
            .first_byte()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value
            .last_byte()
            .is_some_and(|byte| byte.is_ascii_alphanumeric());

    if !valid {
        return Err(ValueObjectError::InvalidFormat {
            kind: ValueKind::DomainAlias,
            expected: EXPECTED,
        });
    }
    Ok(())
}

fn validate_database_name(value: &str) -> Result<(), ValueObjectError> {
    const BLOCKED: [&str; 4] = ["mysql", "information_schema", "performance_schema", "sys"];
    validate_simple_identifier(
        value,
        ValueKind::DatabaseName,
        64,
        "1-64 ASCII letters, digits or `_`, starting with a letter or digit",
        |byte| byte.is_ascii_alphanumeric() || byte == b'_',
    )?;

    if BLOCKED
        .iter()
        .any(|blocked| value.eq_ignore_ascii_case(blocked))
    {
        return Err(ValueObjectError::Reserved {
            kind: ValueKind::DatabaseName,
        });
    }
    Ok(())
}

fn validate_container_name(value: &str) -> Result<(), ValueObjectError> {
    validate_simple_identifier(
        value,
        ValueKind::ContainerName,
        128,
        "1-128 ASCII letters, digits, `_`, `.` or `-`, starting with a letter or digit",
        |byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'),
    )
}

fn validate_container_id(value: &str) -> Result<(), ValueObjectError> {
    const EXPECTED: &str = "a full 64-character lowercase hexadecimal Docker ID";
    validate_length(value, ValueKind::ContainerId, 64)?;
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(ValueObjectError::InvalidFormat {
            kind: ValueKind::ContainerId,
            expected: EXPECTED,
        });
    }
    Ok(())
}

fn validate_simple_identifier(
    value: &str,
    kind: ValueKind,
    max: usize,
    expected: &'static str,
    allowed: impl Fn(u8) -> bool,
) -> Result<(), ValueObjectError> {
    validate_length(value, kind, max)?;
    let mut bytes = value.bytes();
    let starts_correctly = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric());
    if !starts_correctly || !bytes.all(allowed) {
        return Err(ValueObjectError::InvalidFormat { kind, expected });
    }
    Ok(())
}

fn validate_length(value: &str, kind: ValueKind, max: usize) -> Result<(), ValueObjectError> {
    if value.is_empty() {
        return Err(ValueObjectError::Empty { kind });
    }
    if value.chars().count() > max {
        return Err(ValueObjectError::TooLong { kind, max });
    }
    Ok(())
}

trait StringBoundaryBytes {
    fn first_byte(&self) -> Option<u8>;
    fn last_byte(&self) -> Option<u8>;
}

impl StringBoundaryBytes for str {
    fn first_byte(&self) -> Option<u8> {
        self.as_bytes().first().copied()
    }

    fn last_byte(&self) -> Option<u8> {
        self.as_bytes().last().copied()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CredentialScope {
    Source,
    Target,
}

impl fmt::Display for CredentialScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Source => "source",
            Self::Target => "target",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CredentialKey {
    scope: CredentialScope,
    id: Uuid,
}

impl CredentialKey {
    pub fn new(scope: CredentialScope) -> Self {
        Self {
            scope,
            id: Uuid::new_v4(),
        }
    }

    pub const fn scope(self) -> CredentialScope {
        self.scope
    }

    pub const fn id(self) -> Uuid {
        self.id
    }
}

impl fmt::Display for CredentialKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.scope, self.id)
    }
}

impl FromStr for CredentialKey {
    type Err = ValueObjectError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (scope, id) = value
            .split_once(':')
            .ok_or(ValueObjectError::InvalidFormat {
                kind: ValueKind::CredentialKey,
                expected: "`source:<uuid>` or `target:<uuid>`",
            })?;
        let scope = match scope {
            "source" => CredentialScope::Source,
            "target" => CredentialScope::Target,
            _ => {
                return Err(ValueObjectError::InvalidFormat {
                    kind: ValueKind::CredentialKey,
                    expected: "`source:<uuid>` or `target:<uuid>`",
                });
            }
        };
        let parsed_id = Uuid::parse_str(id).map_err(|_| ValueObjectError::InvalidFormat {
            kind: ValueKind::CredentialKey,
            expected: "`source:<uuid>` or `target:<uuid>`",
        })?;
        if parsed_id.hyphenated().to_string() != id {
            return Err(ValueObjectError::InvalidFormat {
                kind: ValueKind::CredentialKey,
                expected: "`source:<uuid>` or `target:<uuid>`",
            });
        }
        Ok(Self {
            scope,
            id: parsed_id,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MysqlVersion {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

impl fmt::Display for MysqlVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl FromStr for MysqlVersion {
    type Err = ValueObjectError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = || ValueObjectError::InvalidFormat {
            kind: ValueKind::MysqlVersion,
            expected: "three numeric components such as `8.4.4`",
        };
        let mut components = value.split('.');
        let major = components.next().ok_or_else(invalid)?;
        let minor = components.next().ok_or_else(invalid)?;
        let patch = components.next().ok_or_else(invalid)?;
        if components.next().is_some()
            || [major, minor, patch].iter().any(|component| {
                component.is_empty() || !component.bytes().all(|b| b.is_ascii_digit())
            })
        {
            return Err(invalid());
        }
        Ok(Self {
            major: major.parse().map_err(|_| invalid())?,
            minor: minor.parse().map_err(|_| invalid())?,
            patch: patch.parse().map_err(|_| invalid())?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_observed_salt_identifiers() {
        assert!(ProfileName::try_from("salt-local").is_ok());
        assert!(TenantLookup::try_from("watt").is_ok());
        assert!(TenantId::try_from("salt_watt_construtora").is_ok());
        assert!(DomainAlias::try_from("sagatec").is_ok());
        assert!(DatabaseName::try_from("salt_watt_construtora").is_ok());
        assert!(ContainerName::try_from("mysql-8").is_ok());
    }

    #[test]
    fn rejects_empty_and_oversized_values() {
        assert!(matches!(
            ProfileName::try_from(""),
            Err(ValueObjectError::Empty { .. })
        ));
        assert!(matches!(
            DatabaseName::try_from("a".repeat(65)),
            Err(ValueObjectError::TooLong { max: 64, .. })
        ));
        assert!(matches!(
            DomainAlias::try_from("a".repeat(64)),
            Err(ValueObjectError::TooLong { max: 63, .. })
        ));
    }

    #[test]
    fn rejects_injection_and_path_inputs() {
        for value in [
            "../../mysql",
            "tenant;DROP DATABASE mysql",
            "tenant name",
            "tenant`name",
            "tenant/name",
            "tenant\0name",
        ] {
            assert!(TenantLookup::try_from(value).is_err(), "{value:?}");
            assert!(DatabaseName::try_from(value).is_err(), "{value:?}");
        }
    }

    #[test]
    fn blocks_administrative_databases_case_insensitively() {
        for value in ["mysql", "INFORMATION_SCHEMA", "Performance_Schema", "sys"] {
            assert!(matches!(
                DatabaseName::try_from(value),
                Err(ValueObjectError::Reserved {
                    kind: ValueKind::DatabaseName
                })
            ));
        }
    }

    #[test]
    fn domain_alias_is_a_lowercase_dns_label() {
        for invalid in ["Salt", "-salt", "salt-", "salt.localhost", "salt_test"] {
            assert!(DomainAlias::try_from(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn container_id_requires_the_full_lowercase_digest() {
        let valid = "a0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcde";
        assert_eq!(valid.len(), 64);
        assert!(ContainerId::try_from(valid).is_ok());
        assert!(ContainerId::try_from(&valid[..12]).is_err());
        assert!(ContainerId::try_from(valid.to_uppercase()).is_err());
    }

    #[test]
    fn credential_key_roundtrips_with_an_explicit_scope() {
        let key = CredentialKey::new(CredentialScope::Source);
        let parsed: CredentialKey = key.to_string().parse().unwrap();

        assert_eq!(parsed, key);
        assert_eq!(parsed.scope(), CredentialScope::Source);
        assert!(
            "profile:550e8400-e29b-41d4-a716-446655440000"
                .parse::<CredentialKey>()
                .is_err()
        );
        assert!(
            "source:550E8400-E29B-41D4-A716-446655440000"
                .parse::<CredentialKey>()
                .is_err()
        );
        assert!(
            "source:550e8400e29b41d4a716446655440000"
                .parse::<CredentialKey>()
                .is_err()
        );
    }

    #[test]
    fn parses_only_exact_mysql_versions() {
        let version: MysqlVersion = "8.4.4".parse().unwrap();
        assert_eq!(version.to_string(), "8.4.4");

        for invalid in ["8.4", "8.4.4.1", "8.4.x", "8.4.4-commercial", ""] {
            assert!(invalid.parse::<MysqlVersion>().is_err(), "{invalid}");
        }
    }

    #[test]
    fn validation_errors_do_not_repeat_untrusted_input() {
        let marker = "sensitive-marker;DROP DATABASE mysql";
        let error = DatabaseName::try_from(marker).unwrap_err();

        assert!(!error.to_string().contains(marker));
        assert!(!format!("{error:?}").contains(marker));
    }
}
