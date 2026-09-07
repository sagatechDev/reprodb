use std::{ffi::OsString, path::Path};

use async_trait::async_trait;

use crate::{
    application::{AuthorizedLocalTarget, LocalTenantWriteError, LocalTenantWriter},
    domain::{LocalTenantFeatures, LocalTenantRegistration},
    infrastructure::{
        credentials::{
            MYSQL_OPTION_FILE_CONTAINER_PATH, MYSQL_SECRETS_CONTAINER_DIRECTORY, MysqlOptionFile,
        },
        process::{ProcessOutput, ProcessRunner, ProcessSpec},
    },
};

const PREFLIGHT_COLUMNS: usize = 3;

pub struct DockerLocalTenantWriter<R> {
    runner: R,
}

impl<R> DockerLocalTenantWriter<R> {
    pub const fn new(runner: R) -> Self {
        Self { runner }
    }
}

#[async_trait]
impl<R> LocalTenantWriter for DockerLocalTenantWriter<R>
where
    R: ProcessRunner,
{
    async fn register(
        &self,
        target: &AuthorizedLocalTarget,
        registration: &LocalTenantRegistration,
    ) -> Result<(), LocalTenantWriteError> {
        let option_file = target_option_file(target)?;
        let preflight = self
            .runner
            .output(&mysql_query_spec(
                target,
                option_file.path(),
                &preflight_query(registration),
            ))
            .await
            .map_err(|_| LocalTenantWriteError::TargetUnavailable)?;
        if !preflight.success {
            return Err(classify_process_failure(&preflight));
        }
        parse_preflight(&preflight.stdout)?;

        let transaction = self
            .runner
            .output(&mysql_query_spec(
                target,
                option_file.path(),
                &registration_transaction(registration),
            ))
            .await
            .map_err(|_| LocalTenantWriteError::TargetUnavailable)?;
        if !transaction.success {
            return Err(classify_process_failure(&transaction));
        }
        if transaction.stdout != b"1\n" {
            return Err(LocalTenantWriteError::TransactionFailed);
        }
        Ok(())
    }
}

fn target_option_file(
    target: &AuthorizedLocalTarget,
) -> Result<MysqlOptionFile, LocalTenantWriteError> {
    MysqlOptionFile::create(
        "127.0.0.1",
        3306,
        target.username(),
        target.password(),
        crate::domain::MysqlTlsMode::Required,
    )
    .map_err(|_| LocalTenantWriteError::TargetUnavailable)
}

fn mysql_query_spec(
    target: &AuthorizedLocalTarget,
    option_file: &Path,
    query: &str,
) -> ProcessSpec {
    let mut mount = OsString::from("type=bind,src=");
    mount.push(option_file.parent().unwrap_or(option_file).as_os_str());
    mount.push(format!(",dst={MYSQL_SECRETS_CONTAINER_DIRECTORY},readonly"));

    let mut spec = ProcessSpec::new("docker").args([
        OsString::from("--context"),
        OsString::from(target.docker_context()),
        OsString::from("run"),
        OsString::from("--rm"),
        OsString::from("--pull=never"),
        OsString::from(format!("--network=container:{}", target.container_id())),
        OsString::from("--mount"),
        mount,
        OsString::from(target.client().image()),
        OsString::from("mysql"),
        OsString::from(format!(
            "--defaults-file={MYSQL_OPTION_FILE_CONTAINER_PATH}"
        )),
    ]);
    if target.client().supports_no_login_paths() {
        spec = spec.arg("--no-login-paths");
    }
    spec.args([
        OsString::from("--batch"),
        OsString::from("--skip-column-names"),
        OsString::from(format!("--database={}", target.central_database())),
        OsString::from("--execute"),
        OsString::from(query),
    ])
}

fn preflight_query(registration: &LocalTenantRegistration) -> String {
    let tenant = hex_utf8(registration.tenant_id().as_str());
    let domain = hex_utf8(registration.local_domain().as_str());
    format!(
        "SET @reprodb_tenant = CONVERT(0x{tenant} USING utf8mb4);\
         SET @reprodb_domain = CONVERT(0x{domain} USING utf8mb4);\
         SELECT\
         ((SELECT COUNT(*) FROM information_schema.COLUMNS \
            WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'tenants' \
              AND COLUMN_NAME IN ('id','created_at','updated_at','data')) = 4 \
          AND (SELECT COUNT(*) FROM information_schema.COLUMNS \
            WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'domains' \
              AND COLUMN_NAME IN ('id','domain','tenant_id','created_at','updated_at')) = 5 \
          AND EXISTS (SELECT 1 FROM information_schema.COLUMNS \
            WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'tenants' AND COLUMN_NAME = 'id' \
              AND DATA_TYPE = 'varchar' AND IS_NULLABLE = 'NO' AND CHARACTER_MAXIMUM_LENGTH >= 191) \
          AND EXISTS (SELECT 1 FROM information_schema.COLUMNS \
            WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'tenants' AND COLUMN_NAME = 'data' \
              AND DATA_TYPE = 'json') \
          AND EXISTS (SELECT 1 FROM information_schema.STATISTICS \
            WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'domains' \
              AND COLUMN_NAME = 'domain' AND NON_UNIQUE = 0) \
          AND EXISTS (SELECT 1 FROM information_schema.KEY_COLUMN_USAGE \
            WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'domains' \
              AND COLUMN_NAME = 'tenant_id' AND REFERENCED_TABLE_NAME = 'tenants' \
              AND REFERENCED_COLUMN_NAME = 'id')),\
         EXISTS(SELECT 1 FROM domains \
           WHERE BINARY domain = BINARY @reprodb_domain \
             AND BINARY tenant_id <> BINARY @reprodb_tenant),\
         EXISTS(SELECT 1 FROM tenants \
           WHERE BINARY id = BINARY @reprodb_tenant AND {blocked_override})",
        blocked_override = blocked_override_expression("data"),
    )
}

fn registration_transaction(registration: &LocalTenantRegistration) -> String {
    let tenant = hex_utf8(registration.tenant_id().as_str());
    let domain = hex_utf8(registration.local_domain().as_str());
    let database = hex_utf8(registration.target_database().as_str());
    let (object_pairs, set_pairs) = feature_json_pairs(registration.features());
    format!(
        "SET @reprodb_tenant = CONVERT(0x{tenant} USING utf8mb4);\
         SET @reprodb_domain = CONVERT(0x{domain} USING utf8mb4);\
         SET @reprodb_database = CONVERT(0x{database} USING utf8mb4);\
         START TRANSACTION;\
         INSERT INTO tenants (id, created_at, updated_at, data) \
         VALUES (@reprodb_tenant, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, \
           JSON_OBJECT('tenancy_db_name', @reprodb_database{object_pairs})) \
         ON DUPLICATE KEY UPDATE \
           id = IF(NOT ({blocked_override}), id, NULL), \
           updated_at = CURRENT_TIMESTAMP, \
           data = JSON_SET(COALESCE(data, JSON_OBJECT()), \
             '$.tenancy_db_name', @reprodb_database{set_pairs});\
         INSERT INTO domains (domain, tenant_id, created_at, updated_at) \
         VALUES (@reprodb_domain, @reprodb_tenant, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP) \
         ON DUPLICATE KEY UPDATE \
           tenant_id = IF(BINARY tenant_id = BINARY VALUES(tenant_id), tenant_id, NULL), \
           updated_at = CURRENT_TIMESTAMP;\
         COMMIT;\
         SELECT 1",
        blocked_override = blocked_override_expression("data"),
    )
}

fn blocked_override_expression(column: &str) -> String {
    [
        "tenancy_db_connection",
        "tenancy_db_host",
        "tenancy_db_port",
        "tenancy_db_username",
        "tenancy_db_password",
        "tenancy_salt_historical_table_name",
    ]
    .into_iter()
    .map(|key| {
        format!(
            "(JSON_EXTRACT({column}, '$.{key}') IS NOT NULL AND JSON_TYPE(JSON_EXTRACT({column}, '$.{key}')) <> 'NULL')"
        )
    })
    .collect::<Vec<_>>()
    .join(" OR ")
}

fn feature_json_pairs(features: &LocalTenantFeatures) -> (String, String) {
    let mut object = Vec::new();
    let mut set = Vec::new();
    if let Some(color) = &features.app_color {
        let value = format!("CONVERT(0x{} USING utf8mb4)", hex_utf8(color.as_str()));
        object.push(format!("'tenancy_app_color', {value}"));
        set.push(format!("'$.tenancy_app_color', {value}"));
    }
    for (key, value) in [
        ("tenancy_annotation_atm", features.annotation_atm),
        (
            "tenancy_enable_stock_label_control",
            features.enable_stock_label_control,
        ),
        ("tenancy_enable_sped_contrib", features.enable_sped_contrib),
        ("tenancy_enable_beta", features.enable_beta),
        ("tenancy_has_cyclic_counting", features.has_cyclic_counting),
        ("tenancy_new_production", features.new_production),
    ] {
        if let Some(value) = value {
            let json = if value {
                "JSON_EXTRACT('true', '$')"
            } else {
                "JSON_EXTRACT('false', '$')"
            };
            object.push(format!("'{key}', {json}"));
            set.push(format!("'$.{key}', {json}"));
        }
    }
    let object = object.into_iter().map(|pair| format!(", {pair}")).collect();
    let set = set.into_iter().map(|pair| format!(", {pair}")).collect();
    (object, set)
}

fn parse_preflight(output: &[u8]) -> Result<(), LocalTenantWriteError> {
    let output = std::str::from_utf8(output)
        .map_err(|_| LocalTenantWriteError::IncompatibleSchema)?
        .trim_end_matches(['\r', '\n']);
    let columns = output.split('\t').collect::<Vec<_>>();
    if columns.len() != PREFLIGHT_COLUMNS
        || !columns.iter().all(|value| matches!(*value, "0" | "1"))
    {
        return Err(LocalTenantWriteError::IncompatibleSchema);
    }
    if columns[0] != "1" {
        Err(LocalTenantWriteError::IncompatibleSchema)
    } else if columns[1] == "1" {
        Err(LocalTenantWriteError::DomainConflict)
    } else if columns[2] == "1" {
        Err(LocalTenantWriteError::ExistingConnectionOverride)
    } else {
        Ok(())
    }
}

fn classify_process_failure(output: &ProcessOutput) -> LocalTenantWriteError {
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    if stderr.contains("access denied") {
        LocalTenantWriteError::AuthenticationFailed
    } else if stderr.contains("can't connect")
        || stderr.contains("connection refused")
        || stderr.contains("cannot connect to the docker daemon")
    {
        LocalTenantWriteError::TargetUnavailable
    } else {
        LocalTenantWriteError::TransactionFailed
    }
}

fn hex_utf8(value: &str) -> String {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut output = String::with_capacity(value.len() * 2);
    for byte in value.bytes() {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use secrecy::SecretString;

    use crate::{
        domain::{AppColor, DatabaseName, DomainAlias, TenantId},
        infrastructure::process::ProcessError,
    };

    use super::*;

    struct FakeRunner {
        outputs: Mutex<VecDeque<ProcessOutput>>,
        specs: Arc<Mutex<Vec<ProcessSpec>>>,
    }

    #[async_trait]
    impl ProcessRunner for FakeRunner {
        async fn output(&self, spec: &ProcessSpec) -> Result<ProcessOutput, ProcessError> {
            self.specs.lock().unwrap().push(spec.clone());
            Ok(self.outputs.lock().unwrap().pop_front().unwrap())
        }
    }

    fn registration() -> LocalTenantRegistration {
        LocalTenantRegistration::try_new(
            TenantId::try_from("salt_sagatec").unwrap(),
            DomainAlias::try_from("sagatec").unwrap(),
            DatabaseName::try_from("salt_sagatec").unwrap(),
            LocalTenantFeatures {
                app_color: Some(AppColor::try_from("green".to_owned()).unwrap()),
                enable_stock_label_control: Some(true),
                enable_beta: Some(false),
                ..LocalTenantFeatures::default()
            },
        )
        .unwrap()
    }

    #[tokio::test]
    async fn uses_two_structured_queries_and_only_hex_encoded_values() {
        let specs = Arc::new(Mutex::new(Vec::new()));
        let writer = DockerLocalTenantWriter::new(FakeRunner {
            outputs: Mutex::new(VecDeque::from([
                ProcessOutput::success("1\t0\t0\n"),
                ProcessOutput::success("1\n"),
            ])),
            specs: Arc::clone(&specs),
        });
        let target =
            AuthorizedLocalTarget::for_test(DatabaseName::try_from("salt_sagatec").unwrap());

        writer.register(&target, &registration()).await.unwrap();

        let specs = specs.lock().unwrap();
        assert_eq!(specs.len(), 2);
        for spec in specs.iter() {
            let arguments = spec
                .arguments()
                .iter()
                .map(|value| value.to_string_lossy())
                .collect::<Vec<_>>();
            assert_eq!(spec.program(), "docker");
            assert!(arguments.contains(&std::borrow::Cow::Borrowed("--database=salt_central")));
            assert!(!arguments.join(" ").contains("local-test-password"));
            assert!(!arguments.last().unwrap().contains("salt_sagatec"));
            assert!(!arguments.last().unwrap().contains("sagatec"));
        }
        let transaction = specs[1].arguments().last().unwrap().to_string_lossy();
        assert!(transaction.contains("START TRANSACTION"));
        assert!(transaction.contains("COMMIT"));
        assert!(transaction.contains("tenancy_db_name"));
        assert!(transaction.contains("tenancy_enable_stock_label_control"));
        assert!(!transaction.contains("tenant_links"));
        assert!(!transaction.contains("tenancy_api_"));
    }

    #[tokio::test]
    async fn preflight_conflict_stops_before_the_transaction() {
        for (output, expected) in [
            ("1\t1\t0\n", LocalTenantWriteError::DomainConflict),
            (
                "1\t0\t1\n",
                LocalTenantWriteError::ExistingConnectionOverride,
            ),
        ] {
            let specs = Arc::new(Mutex::new(Vec::new()));
            let writer = DockerLocalTenantWriter::new(FakeRunner {
                outputs: Mutex::new(VecDeque::from([ProcessOutput::success(output)])),
                specs: Arc::clone(&specs),
            });
            let target =
                AuthorizedLocalTarget::for_test(DatabaseName::try_from("salt_sagatec").unwrap());

            assert_eq!(
                writer.register(&target, &registration()).await,
                Err(expected)
            );
            assert_eq!(specs.lock().unwrap().len(), 1);
        }
    }

    #[test]
    fn distinguishes_schema_domain_and_connection_override_before_mutation() {
        assert_eq!(
            parse_preflight(b"0\t0\t0\n"),
            Err(LocalTenantWriteError::IncompatibleSchema)
        );
        assert_eq!(
            parse_preflight(b"1\t1\t0\n"),
            Err(LocalTenantWriteError::DomainConflict)
        );
        assert_eq!(
            parse_preflight(b"1\t0\t1\n"),
            Err(LocalTenantWriteError::ExistingConnectionOverride)
        );
        assert_eq!(
            parse_preflight(b"1\t0\t0\textra\n"),
            Err(LocalTenantWriteError::IncompatibleSchema)
        );
    }

    #[test]
    fn external_diagnostics_are_classified_without_being_returned() {
        let marker = "password-that-must-not-leak";
        let output = ProcessOutput::failure(1, format!("Access denied {marker}"));
        let error = classify_process_failure(&output);
        assert_eq!(error, LocalTenantWriteError::AuthenticationFailed);
        assert!(!error.to_string().contains(marker));
        assert!(!format!("{error:?}").contains(marker));
    }

    #[test]
    fn password_never_participates_in_sql_generation() {
        let _password = SecretString::from("password-that-must-not-leak");
        let sql = registration_transaction(&registration());
        assert!(!sql.contains("password-that-must-not-leak"));
    }
}
