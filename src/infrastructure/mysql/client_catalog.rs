use thiserror::Error;

use crate::domain::MysqlVersion;

pub const MYSQL_CLIENT_CATALOG_VERSION: u32 = 2;

const SUPPORTED_MYSQL_SERIES: &str = "8.0, 8.4";

const MYSQL_8_0_46_IMAGE: &str = concat!(
    "mysql:8.0.46@sha256:",
    "7dcddc01f13bab2f15cde676d44d01f61fc9f99fe7785e86196dfc07d358ae2b"
);
const MYSQL_8_0_46_REPO_DIGEST: &str = concat!(
    "mysql@sha256:",
    "7dcddc01f13bab2f15cde676d44d01f61fc9f99fe7785e86196dfc07d358ae2b"
);

const MYSQL_8_4_4_IMAGE: &str = concat!(
    "mysql:8.4.4@sha256:",
    "1d967fb75a64dc3c2894c69285becfc2304ae0c3c4f4c715c297f3c12d60b01c"
);
const MYSQL_8_4_4_REPO_DIGEST: &str = concat!(
    "mysql@sha256:",
    "1d967fb75a64dc3c2894c69285becfc2304ae0c3c4f4c715c297f3c12d60b01c"
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ApprovedMysqlClient {
    series: &'static str,
    version: MysqlVersion,
    image: &'static str,
    repository_digest: &'static str,
    supports_no_login_paths: bool,
}

impl ApprovedMysqlClient {
    pub const fn series(self) -> &'static str {
        self.series
    }

    pub const fn version(self) -> MysqlVersion {
        self.version
    }

    pub const fn image(self) -> &'static str {
        self.image
    }

    pub const fn repository_digest(self) -> &'static str {
        self.repository_digest
    }

    pub const fn supports_no_login_paths(self) -> bool {
        self.supports_no_login_paths
    }
}

const APPROVED_CLIENTS: [ApprovedMysqlClient; 2] = [
    ApprovedMysqlClient {
        series: "8.0",
        version: MysqlVersion {
            major: 8,
            minor: 0,
            patch: 46,
        },
        image: MYSQL_8_0_46_IMAGE,
        repository_digest: MYSQL_8_0_46_REPO_DIGEST,
        supports_no_login_paths: false,
    },
    ApprovedMysqlClient {
        series: "8.4",
        version: MysqlVersion {
            major: 8,
            minor: 4,
            patch: 4,
        },
        image: MYSQL_8_4_4_IMAGE,
        repository_digest: MYSQL_8_4_4_REPO_DIGEST,
        supports_no_login_paths: true,
    },
];

#[derive(Clone, Copy, Debug, Default)]
pub struct ClientCatalog;

impl ClientCatalog {
    pub const fn version() -> u32 {
        MYSQL_CLIENT_CATALOG_VERSION
    }

    pub const fn supported_series() -> &'static str {
        SUPPORTED_MYSQL_SERIES
    }

    pub fn resolve(series: &str) -> Result<ApprovedMysqlClient, ClientCatalogError> {
        APPROVED_CLIENTS
            .iter()
            .copied()
            .find(|client| client.series == series)
            .ok_or(ClientCatalogError::UnsupportedSeries)
    }

    pub fn validate(
        series: &str,
        configured_image: &str,
    ) -> Result<ApprovedMysqlClient, ClientCatalogError> {
        let client = Self::resolve(series)?;
        if client.image != configured_image {
            return Err(ClientCatalogError::UnapprovedImage);
        }
        Ok(client)
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ClientCatalogError {
    #[error("this MySQL series has no client approved by reprodb")]
    UnsupportedSeries,

    #[error("the configured MySQL client image is not the approved tag and digest")]
    UnapprovedImage,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_the_locally_tested_mysql_client() {
        let client = ClientCatalog::resolve("8.4").unwrap();

        assert_eq!(ClientCatalog::version(), 2);
        assert_eq!(ClientCatalog::supported_series(), "8.0, 8.4");
        assert_eq!(client.version().to_string(), "8.4.4");
        assert!(client.image().starts_with("mysql:8.4.4@sha256:"));
        assert_eq!(client.image().matches("sha256:").count(), 1);
        assert_eq!(client.repository_digest().len(), "mysql@sha256:".len() + 64);
        assert!(client.supports_no_login_paths());
    }

    #[test]
    fn rejects_unknown_series_and_mutable_or_divergent_images() {
        let mysql_8_0 = ClientCatalog::resolve("8.0").unwrap();
        assert_eq!(mysql_8_0.version().to_string(), "8.0.46");
        assert!(mysql_8_0.image().starts_with("mysql:8.0.46@sha256:"));
        assert!(!mysql_8_0.supports_no_login_paths());
        assert_eq!(
            ClientCatalog::resolve("5.7").unwrap_err(),
            ClientCatalogError::UnsupportedSeries
        );

        for image in [
            "mysql:8",
            "mysql:8.4.4",
            "mysql:8.4.4@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert_eq!(
                ClientCatalog::validate("8.4", image).unwrap_err(),
                ClientCatalogError::UnapprovedImage
            );
        }
    }

    #[test]
    fn errors_do_not_echo_an_untrusted_image_reference() {
        let marker = "image; password-that-must-not-leak";
        let error = ClientCatalog::validate("8.4", marker).unwrap_err();

        assert!(!error.to_string().contains(marker));
        assert!(!format!("{error:?}").contains(marker));
    }
}
