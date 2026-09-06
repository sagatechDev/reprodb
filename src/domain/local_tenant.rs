use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::{
    DatabaseName, DomainAlias, DumpArtifactMetadata, ResolvedTenant, TenantId, TenantLookup,
    TenantMatch, ValueObjectError,
};

const MYSQL_TENANT_ID_MAX_BYTES: usize = 191;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct AppColor(String);

impl AppColor {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for AppColor {
    type Error = LocalTenantRegistrationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let named = matches!(
            value.as_str(),
            "blue" | "blueLight" | "gray" | "green" | "orange" | "violet"
        );
        let hexadecimal = value.len() == 7
            && value.starts_with('#')
            && value[1..].bytes().all(|byte| byte.is_ascii_hexdigit());
        if named || hexadecimal {
            Ok(Self(value))
        } else {
            Err(LocalTenantRegistrationError::InvalidAppColor)
        }
    }
}

impl From<AppColor> for String {
    fn from(value: AppColor) -> Self {
        value.0
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalTenantFeatures {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_color: Option<AppColor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotation_atm: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_stock_label_control: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_sped_contrib: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_beta: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_cyclic_counting: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_production: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalTenantRegistration {
    tenant_id: TenantId,
    local_domain: DomainAlias,
    target_database: DatabaseName,
    features: LocalTenantFeatures,
}

impl LocalTenantRegistration {
    pub fn try_new(
        tenant_id: TenantId,
        local_domain: DomainAlias,
        target_database: DatabaseName,
        features: LocalTenantFeatures,
    ) -> Result<Self, LocalTenantRegistrationError> {
        if tenant_id.as_str().len() > MYSQL_TENANT_ID_MAX_BYTES {
            return Err(LocalTenantRegistrationError::TenantIdTooLong);
        }
        if local_domain.as_str() == "localhost" {
            return Err(LocalTenantRegistrationError::CentralDomain);
        }
        Ok(Self {
            tenant_id,
            local_domain,
            target_database,
            features,
        })
    }

    pub fn from_resolution(
        lookup: &TenantLookup,
        resolved: &ResolvedTenant,
    ) -> Result<Self, LocalTenantRegistrationError> {
        let local_domain = match resolved.matched_by {
            TenantMatch::Domain => DomainAlias::try_from(lookup.as_str())?,
            TenantMatch::TenantId | TenantMatch::Pattern => {
                let normalized = resolved
                    .tenant_id
                    .as_str()
                    .to_ascii_lowercase()
                    .replace('_', "-");
                DomainAlias::try_from(normalized)?
            }
        };
        Self::try_new(
            resolved.tenant_id.clone(),
            local_domain,
            resolved.database.clone(),
            resolved.features.clone(),
        )
    }

    pub fn from_artifact(
        metadata: &DumpArtifactMetadata,
    ) -> Result<Self, LocalTenantRegistrationError> {
        let resolved = ResolvedTenant {
            tenant_id: metadata.tenant_id.clone(),
            database: metadata.database.clone(),
            matched_by: if metadata.tenant_lookup.as_str() == metadata.tenant_id.as_str() {
                TenantMatch::TenantId
            } else {
                TenantMatch::Domain
            },
            features: metadata.local_tenant_features.clone(),
        };
        Self::from_resolution(&metadata.tenant_lookup, &resolved)
    }

    pub fn tenant_id(&self) -> &TenantId {
        &self.tenant_id
    }

    pub fn local_domain(&self) -> &DomainAlias {
        &self.local_domain
    }

    pub fn target_database(&self) -> &DatabaseName {
        &self.target_database
    }

    pub fn features(&self) -> &LocalTenantFeatures {
        &self.features
    }
}

#[derive(Debug, Error)]
pub enum LocalTenantRegistrationError {
    #[error("the tenant ID exceeds the local Salt central schema limit")]
    TenantIdTooLong,
    #[error("the local tenant domain cannot be a central domain")]
    CentralDomain,
    #[error("the tenant app color is not in the local allowlist")]
    InvalidAppColor,
    #[error(transparent)]
    InvalidDomain(#[from] ValueObjectError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(matched_by: TenantMatch) -> ResolvedTenant {
        ResolvedTenant {
            tenant_id: TenantId::try_from("salt_sagatec").unwrap(),
            database: DatabaseName::try_from("salt_sagatec").unwrap(),
            matched_by,
            features: LocalTenantFeatures::default(),
        }
    }

    #[test]
    fn reuses_a_valid_domain_lookup_and_derives_one_from_a_tenant_id() {
        let by_domain = LocalTenantRegistration::from_resolution(
            &TenantLookup::try_from("sagatec").unwrap(),
            &resolved(TenantMatch::Domain),
        )
        .unwrap();
        let by_id = LocalTenantRegistration::from_resolution(
            &TenantLookup::try_from("salt_sagatec").unwrap(),
            &resolved(TenantMatch::TenantId),
        )
        .unwrap();

        assert_eq!(by_domain.local_domain().as_str(), "sagatec");
        assert_eq!(by_id.local_domain().as_str(), "salt-sagatec");
    }

    #[test]
    fn accepts_only_explicit_theme_values_and_hex_colors() {
        for value in [
            "blue",
            "blueLight",
            "gray",
            "green",
            "orange",
            "violet",
            "#123456",
        ] {
            assert!(AppColor::try_from(value.to_owned()).is_ok());
        }
        for value in ["", "red", "#12345g", "blue; color:red"] {
            assert!(AppColor::try_from(value.to_owned()).is_err());
        }
    }

    #[test]
    fn rejects_the_central_domain_and_ids_larger_than_the_real_schema() {
        assert!(matches!(
            LocalTenantRegistration::try_new(
                TenantId::try_from("salt_sagatec").unwrap(),
                DomainAlias::try_from("localhost").unwrap(),
                DatabaseName::try_from("salt_sagatec").unwrap(),
                LocalTenantFeatures::default(),
            ),
            Err(LocalTenantRegistrationError::CentralDomain)
        ));
        assert!(matches!(
            LocalTenantRegistration::try_new(
                TenantId::try_from(format!("s{}", "a".repeat(191))).unwrap(),
                DomainAlias::try_from("sagatec").unwrap(),
                DatabaseName::try_from("salt_sagatec").unwrap(),
                LocalTenantFeatures::default(),
            ),
            Err(LocalTenantRegistrationError::TenantIdTooLong)
        ));
    }
}
