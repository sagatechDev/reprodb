use thiserror::Error;

use crate::domain::{DatabaseName, MysqlVersion};

pub const MYSQL_8_DUMP_POLICY_VERSION: u32 = 2;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DatabaseEncoding {
    charset: String,
    collation: String,
}

impl DatabaseEncoding {
    pub fn try_new(charset: String, collation: String) -> Result<Self, DumpPreflightMetadataError> {
        validate_mysql_name(&charset, 64)
            .then_some(())
            .ok_or(DumpPreflightMetadataError::InvalidCharset)?;
        validate_mysql_name(&collation, 64)
            .then_some(())
            .ok_or(DumpPreflightMetadataError::InvalidCollation)?;
        if !collation.starts_with(&format!("{charset}_")) {
            return Err(DumpPreflightMetadataError::InvalidCollation);
        }

        Ok(Self { charset, collation })
    }

    pub fn charset(&self) -> &str {
        &self.charset
    }

    pub fn collation(&self) -> &str {
        &self.collation
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageEngineUsage {
    engine: String,
    table_count: u64,
}

impl StorageEngineUsage {
    pub fn try_new(engine: String, table_count: u64) -> Result<Self, DumpPreflightMetadataError> {
        if !validate_mysql_name(&engine, 64) || table_count == 0 {
            return Err(DumpPreflightMetadataError::InvalidEngine);
        }
        Ok(Self {
            engine,
            table_count,
        })
    }

    pub fn engine(&self) -> &str {
        &self.engine
    }

    pub const fn table_count(&self) -> u64 {
        self.table_count
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DatabaseObjectCounts {
    pub views: u64,
    pub triggers: u64,
    pub routines: u64,
    pub events: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DefinerObjectCounts {
    pub views: u64,
    pub triggers: u64,
    pub routines: u64,
    pub events: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GtidMode {
    Off,
    OffPermissive,
    OnPermissive,
    On,
}

impl GtidMode {
    pub fn parse(value: &str) -> Result<Self, DumpPreflightMetadataError> {
        match value {
            "OFF" => Ok(Self::Off),
            "OFF_PERMISSIVE" => Ok(Self::OffPermissive),
            "ON_PERMISSIVE" => Ok(Self::OnPermissive),
            "ON" => Ok(Self::On),
            _ => Err(DumpPreflightMetadataError::InvalidGtidMode),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DumpPreflight {
    pub encoding: DatabaseEncoding,
    pub estimated_data_bytes: u64,
    pub engines: Vec<StorageEngineUsage>,
    pub objects: DatabaseObjectCounts,
    pub definers: DefinerObjectCounts,
    pub gtid_mode: GtidMode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovedDumpPlan {
    policy_version: u32,
    arguments: Vec<String>,
    notices: Vec<DumpPolicyNotice>,
}

impl ApprovedDumpPlan {
    pub const fn policy_version(&self) -> u32 {
        self.policy_version
    }

    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }

    pub fn notices(&self) -> &[DumpPolicyNotice] {
        &self.notices
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DumpPolicyNotice {
    ConcurrentDdlMustBePrevented,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Mysql8DumpPolicy;

impl Mysql8DumpPolicy {
    pub fn evaluate(
        server_version: MysqlVersion,
        server_vendor: &str,
        client_version: MysqlVersion,
        database: &DatabaseName,
        preflight: &DumpPreflight,
    ) -> Result<ApprovedDumpPlan, DumpPolicyError> {
        if server_vendor.to_ascii_lowercase().contains("mariadb") {
            return Err(DumpPolicyError::UnsupportedVendor);
        }
        if !matches!(
            (server_version.major, server_version.minor),
            (8, 0) | (8, 4)
        ) {
            return Err(DumpPolicyError::UnsupportedServerSeries);
        }
        if (client_version.major, client_version.minor)
            != (server_version.major, server_version.minor)
        {
            return Err(DumpPolicyError::ClientServerSeriesMismatch);
        }

        let non_innodb_tables = preflight
            .engines
            .iter()
            .filter(|usage| !usage.engine().eq_ignore_ascii_case("InnoDB"))
            .map(StorageEngineUsage::table_count)
            .sum();
        if non_innodb_tables > 0 {
            return Err(DumpPolicyError::NonTransactionalTables {
                count: non_innodb_tables,
            });
        }
        let definer_count = preflight
            .definers
            .views
            .saturating_add(preflight.definers.triggers)
            .saturating_add(preflight.definers.routines)
            .saturating_add(preflight.definers.events);
        if definer_count > 0 {
            return Err(DumpPolicyError::DefinerObjectsUnsupported {
                count: definer_count,
            });
        }
        for (count, error) in [
            (
                preflight.objects.views,
                DumpPolicyError::ViewsUnsupported {
                    count: preflight.objects.views,
                },
            ),
            (
                preflight.objects.triggers,
                DumpPolicyError::TriggersUnsupported {
                    count: preflight.objects.triggers,
                },
            ),
            (
                preflight.objects.routines,
                DumpPolicyError::StoredRoutinesUnsupported {
                    count: preflight.objects.routines,
                },
            ),
            (
                preflight.objects.events,
                DumpPolicyError::EventsUnsupported {
                    count: preflight.objects.events,
                },
            ),
        ] {
            if count > 0 {
                return Err(error);
            }
        }

        Ok(ApprovedDumpPlan {
            policy_version: MYSQL_8_DUMP_POLICY_VERSION,
            arguments: vec![
                "--single-transaction".to_owned(),
                "--quick".to_owned(),
                "--compression-algorithms=zstd,uncompressed".to_owned(),
                "--zstd-compression-level=1".to_owned(),
                "--no-tablespaces".to_owned(),
                "--hex-blob".to_owned(),
                "--set-gtid-purged=OFF".to_owned(),
                "--triggers".to_owned(),
                "--skip-routines".to_owned(),
                "--skip-events".to_owned(),
                "--skip-lock-tables".to_owned(),
                format!("--default-character-set={}", preflight.encoding.charset()),
                database.as_str().to_owned(),
            ],
            notices: vec![DumpPolicyNotice::ConcurrentDdlMustBePrevented],
        })
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum DumpPreflightMetadataError {
    #[error("the source returned an invalid database charset")]
    InvalidCharset,

    #[error("the source returned an invalid database collation")]
    InvalidCollation,

    #[error("the source returned invalid storage engine metadata")]
    InvalidEngine,

    #[error("the source returned an invalid GTID mode")]
    InvalidGtidMode,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum DumpPolicyError {
    #[error("the source vendor is not supported by the MySQL 8 dump policy")]
    UnsupportedVendor,

    #[error("the source server series is not supported; this policy accepts MySQL 8.0 or 8.4")]
    UnsupportedServerSeries,

    #[error("the approved client and source server must use the same MySQL series")]
    ClientServerSeriesMismatch,

    #[error("the database has {count} non-InnoDB table(s); a consistent dump cannot be guaranteed")]
    NonTransactionalTables { count: u64 },

    #[error(
        "the database has {count} stored routine(s); routines are blocked until their restore policy is implemented"
    )]
    StoredRoutinesUnsupported { count: u64 },

    #[error(
        "the database has {count} view(s); views are blocked until their restore semantics are approved"
    )]
    ViewsUnsupported { count: u64 },

    #[error(
        "the database has {count} trigger(s); triggers are blocked until their restore semantics are approved"
    )]
    TriggersUnsupported { count: u64 },

    #[error(
        "the database has {count} object(s) with DEFINER metadata; restoring source authorization identities is not allowed"
    )]
    DefinerObjectsUnsupported { count: u64 },

    #[error(
        "the database has {count} event(s); events are blocked until their restore policy is implemented"
    )]
    EventsUnsupported { count: u64 },
}

fn validate_mysql_name(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(value: &str) -> MysqlVersion {
        value.parse().unwrap()
    }

    fn safe_preflight() -> DumpPreflight {
        DumpPreflight {
            encoding: DatabaseEncoding::try_new(
                "utf8mb4".to_owned(),
                "utf8mb4_0900_ai_ci".to_owned(),
            )
            .unwrap(),
            estimated_data_bytes: 8 * 1024 * 1024,
            engines: vec![StorageEngineUsage::try_new("InnoDB".to_owned(), 42).unwrap()],
            objects: DatabaseObjectCounts::default(),
            definers: DefinerObjectCounts::default(),
            gtid_mode: GtidMode::On,
        }
    }

    #[test]
    fn creates_an_exact_ordered_argument_list_without_a_shell() {
        let database = DatabaseName::try_from("salt_sagatec").unwrap();

        let plan = Mysql8DumpPolicy::evaluate(
            version("8.4.4"),
            "MySQL Community Server - GPL",
            version("8.4.4"),
            &database,
            &safe_preflight(),
        )
        .unwrap();

        assert_eq!(plan.policy_version(), MYSQL_8_DUMP_POLICY_VERSION);
        assert_eq!(
            plan.arguments(),
            [
                "--single-transaction",
                "--quick",
                "--compression-algorithms=zstd,uncompressed",
                "--zstd-compression-level=1",
                "--no-tablespaces",
                "--hex-blob",
                "--set-gtid-purged=OFF",
                "--triggers",
                "--skip-routines",
                "--skip-events",
                "--skip-lock-tables",
                "--default-character-set=utf8mb4",
                "salt_sagatec",
            ]
        );
        assert!(
            plan.arguments()
                .iter()
                .all(|argument| !matches!(argument.as_str(), "sh" | "bash" | "-c"))
        );
        assert_eq!(
            plan.notices(),
            [DumpPolicyNotice::ConcurrentDdlMustBePrevented]
        );
    }

    #[test]
    fn accepts_each_approved_mysql_8_series_with_a_matching_client() {
        let database = DatabaseName::try_from("salt_sagatec").unwrap();

        for supported in ["8.0.46", "8.4.4"] {
            let plan = Mysql8DumpPolicy::evaluate(
                version(supported),
                "MySQL Community Server - GPL",
                version(supported),
                &database,
                &safe_preflight(),
            )
            .unwrap();

            assert_eq!(plan.arguments().last().unwrap(), "salt_sagatec");
        }
    }

    #[test]
    fn blocks_non_innodb_tables_before_building_a_dump() {
        let mut preflight = safe_preflight();
        preflight
            .engines
            .push(StorageEngineUsage::try_new("MyISAM".to_owned(), 3).unwrap());

        let error = Mysql8DumpPolicy::evaluate(
            version("8.4.4"),
            "MySQL Community Server - GPL",
            version("8.4.4"),
            &DatabaseName::try_from("salt_polymer").unwrap(),
            &preflight,
        )
        .unwrap_err();

        assert_eq!(error, DumpPolicyError::NonTransactionalTables { count: 3 });
    }

    #[test]
    fn blocks_routines_and_events_until_restore_semantics_are_defined() {
        let database = DatabaseName::try_from("salt_polymer").unwrap();
        let mut routines = safe_preflight();
        routines.objects.routines = 1;
        assert!(matches!(
            Mysql8DumpPolicy::evaluate(
                version("8.4.4"),
                "MySQL Community Server - GPL",
                version("8.4.4"),
                &database,
                &routines,
            ),
            Err(DumpPolicyError::StoredRoutinesUnsupported { count: 1 })
        ));

        let mut events = safe_preflight();
        events.objects.events = 2;
        assert!(matches!(
            Mysql8DumpPolicy::evaluate(
                version("8.4.4"),
                "MySQL Community Server - GPL",
                version("8.4.4"),
                &database,
                &events,
            ),
            Err(DumpPolicyError::EventsUnsupported { count: 2 })
        ));
    }

    #[test]
    fn blocks_views_triggers_and_definers_until_restore_semantics_are_defined() {
        let database = DatabaseName::try_from("salt_polymer").unwrap();

        let mut definer = safe_preflight();
        definer.objects.views = 1;
        definer.definers.views = 1;
        assert_eq!(
            Mysql8DumpPolicy::evaluate(
                version("8.4.4"),
                "MySQL Community Server - GPL",
                version("8.4.4"),
                &database,
                &definer,
            )
            .unwrap_err(),
            DumpPolicyError::DefinerObjectsUnsupported { count: 1 }
        );

        let mut view = safe_preflight();
        view.objects.views = 1;
        assert_eq!(
            Mysql8DumpPolicy::evaluate(
                version("8.4.4"),
                "MySQL Community Server - GPL",
                version("8.4.4"),
                &database,
                &view,
            )
            .unwrap_err(),
            DumpPolicyError::ViewsUnsupported { count: 1 }
        );

        let mut trigger = safe_preflight();
        trigger.objects.triggers = 1;
        assert_eq!(
            Mysql8DumpPolicy::evaluate(
                version("8.4.4"),
                "MySQL Community Server - GPL",
                version("8.4.4"),
                &database,
                &trigger,
            )
            .unwrap_err(),
            DumpPolicyError::TriggersUnsupported { count: 1 }
        );
    }

    #[test]
    fn requires_mysql_vendor_supported_series_and_matching_client() {
        let database = DatabaseName::try_from("salt_sagatec").unwrap();
        let preflight = safe_preflight();

        assert_eq!(
            Mysql8DumpPolicy::evaluate(
                version("8.4.4"),
                "MariaDB Server",
                version("8.4.4"),
                &database,
                &preflight,
            )
            .unwrap_err(),
            DumpPolicyError::UnsupportedVendor
        );

        assert!(
            Mysql8DumpPolicy::evaluate(
                version("8.0.40"),
                "Source distribution",
                version("8.0.40"),
                &database,
                &preflight,
            )
            .is_ok()
        );
        assert_eq!(
            Mysql8DumpPolicy::evaluate(
                version("5.7.44"),
                "MySQL Community Server - GPL",
                version("8.4.4"),
                &database,
                &preflight,
            )
            .unwrap_err(),
            DumpPolicyError::UnsupportedServerSeries
        );
        assert_eq!(
            Mysql8DumpPolicy::evaluate(
                version("8.4.4"),
                "MySQL Community Server - GPL",
                version("8.0.40"),
                &database,
                &preflight,
            )
            .unwrap_err(),
            DumpPolicyError::ClientServerSeriesMismatch
        );
    }

    #[test]
    fn validates_charset_collation_engine_and_gtid_metadata() {
        assert!(matches!(
            DatabaseEncoding::try_new("utf8mb4;unsafe".to_owned(), "utf8mb4_bin".to_owned()),
            Err(DumpPreflightMetadataError::InvalidCharset)
        ));
        assert!(matches!(
            DatabaseEncoding::try_new("utf8mb4".to_owned(), "latin1_bin".to_owned()),
            Err(DumpPreflightMetadataError::InvalidCollation)
        ));
        assert!(StorageEngineUsage::try_new("InnoDB;unsafe".to_owned(), 1).is_err());
        assert!(StorageEngineUsage::try_new("InnoDB".to_owned(), 0).is_err());
        assert!(GtidMode::parse("ON;unsafe").is_err());
    }
}
