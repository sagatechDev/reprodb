use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use directories::BaseDirs;
use fs4::TryLockError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::domain::{
    ContainerId, ContainerName, CredentialKey, CredentialScope, MysqlTlsMaterialPaths,
    MysqlTlsMode, ProfileName, Sha256Digest,
};
use crate::infrastructure::mysql::{ClientCatalog, ClientCatalogError};

/// Bump this whenever a stored field is added, renamed or removed, and teach
/// [`migrate_to_current_schema`] how to get there from the previous version.
/// Leaving it behind makes an outdated file claim to be current, which strict
/// deserialization then rejects with no way to recover.
pub const CURRENT_SCHEMA_VERSION: u32 = 2;
const CONFIG_FILE_NAME: &str = "reprodb.toml";
const LOCK_FILE_NAME: &str = "reprodb.lock";
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const REPRODB_HOME_ENV: &str = "REPRODB_HOME";
const REPRODB_HOME_DIRECTORY: &str = ".reprodb";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppPaths {
    config_dir: PathBuf,
    cache_dir: PathBuf,
    data_dir: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self, ConfigError> {
        if let Some(configured) = std::env::var_os(REPRODB_HOME_ENV) {
            let root = PathBuf::from(configured);
            if !root.is_absolute() || root.as_os_str().is_empty() {
                return Err(ConfigError::InvalidHomeOverride);
            }
            return Ok(Self::from_root(root));
        }

        let base = BaseDirs::new().ok_or(ConfigError::HomeDirectoryUnavailable)?;
        Ok(Self::from_root(
            base.home_dir().join(REPRODB_HOME_DIRECTORY),
        ))
    }

    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            config_dir: root.clone(),
            cache_dir: root.join("cache"),
            data_dir: root.join("data"),
        }
    }

    pub fn new(
        config_dir: impl Into<PathBuf>,
        cache_dir: impl Into<PathBuf>,
        data_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            config_dir: config_dir.into(),
            cache_dir: cache_dir.into(),
            data_dir: data_dir.into(),
        }
    }

    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join(CONFIG_FILE_NAME)
    }

    pub fn config_lock_file(&self) -> PathBuf {
        self.config_dir.join(LOCK_FILE_NAME)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub schema_version: u32,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_profile: Option<ProfileName>,

    #[serde(default)]
    pub client_runtime: ClientRuntimeConfig,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_target: Option<LocalTargetConfig>,

    /// Configured targets other than the default `local_target`.
    ///
    /// Keeping the default target in its original field preserves compatibility
    /// with configuration files written before multi-target support.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub local_targets: BTreeMap<ContainerName, LocalTargetConfig>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub profiles: BTreeMap<ProfileName, SourceProfileConfig>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            active_profile: None,
            client_runtime: ClientRuntimeConfig::default(),
            local_target: None,
            local_targets: BTreeMap::new(),
            profiles: BTreeMap::new(),
        }
    }
}

impl AppConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version != CURRENT_SCHEMA_VERSION {
            return Err(ConfigError::UnsupportedSchemaVersion {
                found: self.schema_version,
                supported: CURRENT_SCHEMA_VERSION,
            });
        }

        if let Some(active_profile) = &self.active_profile
            && !self.profiles.contains_key(active_profile)
        {
            return Err(ConfigError::ActiveProfileNotFound);
        }

        if let Some(target) = &self.local_target {
            target.validate()?;
            if let Some(runtime_context) = &self.client_runtime.docker_context
                && runtime_context != &target.docker_context
            {
                return Err(ConfigError::InvalidField {
                    field: "local_target.docker_context",
                    reason: "must match the MySQL client runtime Docker context",
                });
            }
        }

        for (name, target) in &self.local_targets {
            target.validate()?;
            if name != &target.container_name {
                return Err(ConfigError::InvalidField {
                    field: "local_targets",
                    reason: "map key must match the configured container name",
                });
            }
            if self
                .local_target
                .as_ref()
                .is_some_and(|active| active.container_name == *name)
            {
                return Err(ConfigError::InvalidField {
                    field: "local_targets",
                    reason: "default target cannot also be stored as an additional target",
                });
            }
            if let Some(runtime_context) = &self.client_runtime.docker_context
                && runtime_context != &target.docker_context
            {
                return Err(ConfigError::InvalidField {
                    field: "local_targets.docker_context",
                    reason: "must match the MySQL client runtime Docker context",
                });
            }
        }

        if let Some(context) = &self.client_runtime.docker_context {
            validate_docker_context("client_runtime.docker_context", context)?;
        }

        for profile in self.profiles.values() {
            profile.validate()?;
        }

        Ok(())
    }

    pub fn configured_local_targets(&self) -> impl Iterator<Item = (&LocalTargetConfig, bool)> {
        self.local_target
            .iter()
            .map(|target| (target, true))
            .chain(self.local_targets.values().map(|target| (target, false)))
    }

    pub fn local_target_named(&self, name: &ContainerName) -> Option<&LocalTargetConfig> {
        self.local_target
            .as_ref()
            .filter(|target| &target.container_name == name)
            .or_else(|| self.local_targets.get(name))
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClientRuntimeKind {
    #[default]
    Docker,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientRuntimeConfig {
    #[serde(rename = "type")]
    pub kind: ClientRuntimeKind,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_context: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalTargetConfig {
    pub docker_context: String,
    pub container_name: ContainerName,
    pub container_id: ContainerId,
    pub username: String,
    pub credential_key: CredentialKey,
    #[serde(default)]
    pub trust: LocalTargetTrust,
}

impl LocalTargetConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        validate_docker_context("local_target.docker_context", &self.docker_context)?;
        validate_plain_text("local_target.username", &self.username, 32)?;
        validate_credential_scope(
            "local_target.credential_key",
            self.credential_key,
            CredentialScope::Target,
        )
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LocalTargetTrust {
    ReprodbManaged,
    #[default]
    UserConfirmed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MysqlFamily {
    Mysql,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceProfileConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub credential_key: CredentialKey,
    pub mysql_family: MysqlFamily,
    pub mysql_series: String,
    pub production: bool,
    #[serde(default)]
    pub tls_mode: MysqlTlsMode,
    #[serde(default, skip_serializing_if = "MysqlTlsMaterialPaths::is_empty")]
    pub tls_material: MysqlTlsMaterialPaths,
    pub client: MysqlClientConfig,
}

impl SourceProfileConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        validate_plain_text("profile.host", &self.host, 255)?;
        if self.port == 0 {
            return Err(ConfigError::InvalidField {
                field: "profile.port",
                reason: "must be between 1 and 65535",
            });
        }
        validate_plain_text("profile.username", &self.username, 32)?;
        validate_credential_scope(
            "profile.credential_key",
            self.credential_key,
            CredentialScope::Source,
        )?;
        validate_mysql_series(&self.mysql_series)?;
        validate_tls_material(self.tls_mode, &self.tls_material)?;
        if self.production && self.tls_mode != MysqlTlsMode::VerifyIdentity {
            return Err(ConfigError::InvalidField {
                field: "profile.tls_mode",
                reason: "must be `verify-identity` for a production source",
            });
        }
        self.client.validate()?;
        match ClientCatalog::validate(&self.mysql_series, &self.client.image) {
            Ok(_) => {}
            Err(ClientCatalogError::UnsupportedSeries) => {
                return Err(ConfigError::InvalidField {
                    field: "profile.mysql_series",
                    reason: "has no client approved by reprodb",
                });
            }
            Err(ClientCatalogError::UnapprovedImage) => {
                return Err(ConfigError::InvalidField {
                    field: "profile.client.image",
                    reason: "does not match the approved tag and digest",
                });
            }
        }
        Ok(())
    }
}

fn validate_tls_material(
    mode: MysqlTlsMode,
    material: &MysqlTlsMaterialPaths,
) -> Result<(), ConfigError> {
    if mode.verifies_certificate_authority() && material.ca.is_none() {
        return Err(ConfigError::InvalidField {
            field: "profile.tls_material.ca",
            reason: "is required when TLS verifies the certificate authority",
        });
    }
    if material.cert.is_some() != material.key.is_some() {
        return Err(ConfigError::InvalidField {
            field: "profile.tls_material",
            reason: "client certificate and key must be configured together",
        });
    }
    if mode == MysqlTlsMode::Disabled && !material.is_empty() {
        return Err(ConfigError::InvalidField {
            field: "profile.tls_material",
            reason: "cannot be configured when TLS is disabled",
        });
    }
    for path in [&material.ca, &material.cert, &material.key]
        .into_iter()
        .flatten()
    {
        if !path.is_absolute() || path.as_os_str().is_empty() {
            return Err(ConfigError::InvalidField {
                field: "profile.tls_material",
                reason: "paths must be absolute",
            });
        }
    }
    Ok(())
}

pub fn source_profile_fingerprint(
    name: &ProfileName,
    profile: &SourceProfileConfig,
) -> Sha256Digest {
    let mut hasher = Sha256::new();
    hasher.update(b"reprodb-source-profile-v1");
    hash_field(&mut hasher, name.as_str().as_bytes());
    hash_field(&mut hasher, profile.host.as_bytes());
    hash_field(&mut hasher, &profile.port.to_be_bytes());
    hash_field(&mut hasher, profile.username.as_bytes());
    match profile.mysql_family {
        MysqlFamily::Mysql => hash_field(&mut hasher, b"mysql"),
    }
    hash_field(&mut hasher, profile.mysql_series.as_bytes());
    hash_field(&mut hasher, profile.tls_mode.option_value().as_bytes());
    hash_field(&mut hasher, profile.client.image.as_bytes());
    hash_field(&mut hasher, &[u8::from(profile.production)]);
    Sha256Digest::from_bytes(hasher.finalize().into())
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MysqlClientConfig {
    pub image: String,
}

impl MysqlClientConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        validate_plain_text("profile.client.image", &self.image, 512)
    }
}

/// Rewrites a stored document in place until it matches the current schema.
///
/// Returns whether anything changed, so the caller can persist the upgrade.
/// Each step is idempotent: running it on an already-current document is a
/// no-op, which keeps a partially migrated file recoverable.
fn migrate_to_current_schema(document: &mut toml::Table, stored_version: u32) -> bool {
    let mut migrated = false;
    if stored_version < 2 {
        migrated |= migrate_v1_to_v2(document);
    }
    if migrated || stored_version < CURRENT_SCHEMA_VERSION {
        document.insert(
            "schema_version".to_owned(),
            toml::Value::Integer(i64::from(CURRENT_SCHEMA_VERSION)),
        );
        migrated = true;
    }
    migrated
}

/// Drops the tenant model that schema 2 removed.
///
/// Both shapes are dead weight rather than data worth keeping: the resolver
/// described how to map a tenant onto a database, and `central_database` named
/// the catalog it was resolved against. Profiles, credentials and the local
/// target survive untouched.
fn migrate_v1_to_v2(document: &mut toml::Table) -> bool {
    let mut migrated = false;
    if let Some(toml::Value::Table(target)) = document.get_mut("local_target") {
        migrated |= target.remove("central_database").is_some();
    }
    if let Some(toml::Value::Table(targets)) = document.get_mut("local_targets") {
        for (_, target) in targets.iter_mut() {
            if let toml::Value::Table(target) = target {
                migrated |= target.remove("central_database").is_some();
            }
        }
    }
    if let Some(toml::Value::Table(profiles)) = document.get_mut("profiles") {
        for (_, profile) in profiles.iter_mut() {
            if let toml::Value::Table(profile) = profile {
                migrated |= profile.remove("tenant_resolver").is_some();
            }
        }
    }
    migrated
}

/// Describes a rejected configuration without quoting the file.
///
/// `toml`'s `Display` renders the offending source line, which would put a
/// stray `password = "..."` straight into an error message and the logs. Only
/// the parser's own message and a line number are safe to surface.
fn invalid_toml(contents: &str, error: &toml::de::Error) -> ConfigError {
    let line = error.span().map_or(1, |span| {
        contents
            .get(..span.start)
            .unwrap_or_default()
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count()
            + 1
    });
    ConfigError::InvalidToml {
        location: format!("the reprodb configuration at line {line}"),
        detail: error.message().to_owned(),
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not determine the user home directory for reprodb")]
    HomeDirectoryUnavailable,

    #[error("REPRODB_HOME must be an absolute, non-empty path")]
    InvalidHomeOverride,

    #[error("could not {operation} the reprodb configuration at {path}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("{location} is not usable: {detail}")]
    InvalidToml { location: String, detail: String },

    #[error("the reprodb configuration is larger than the 1 MiB safety limit")]
    ConfigTooLarge,

    #[error("configuration schema version {found} is not supported; expected {supported}")]
    UnsupportedSchemaVersion { found: u32, supported: u32 },

    #[error("the active profile does not exist in the configured profiles")]
    ActiveProfileNotFound,

    #[error("invalid configuration field `{field}`: {reason}")]
    InvalidField {
        field: &'static str,
        reason: &'static str,
    },

    #[error("another reprodb process is writing the configuration; retry shortly")]
    WriteLocked,

    #[error("could not serialize the reprodb configuration")]
    Serialization,
}

#[derive(Clone, Debug)]
pub struct ConfigRepository {
    paths: AppPaths,
}

impl ConfigRepository {
    pub fn new(paths: AppPaths) -> Self {
        Self { paths }
    }

    pub fn discover() -> Result<Self, ConfigError> {
        Ok(Self::new(AppPaths::discover()?))
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    pub fn load(&self) -> Result<AppConfig, ConfigError> {
        let path = self.paths.config_file();
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Ok(AppConfig::default());
            }
            Err(source) => {
                return Err(ConfigError::Io {
                    operation: "open",
                    path,
                    source,
                });
            }
        };

        if file
            .metadata()
            .map_err(|source| ConfigError::Io {
                operation: "inspect",
                path: path.clone(),
                source,
            })?
            .len()
            > MAX_CONFIG_BYTES
        {
            return Err(ConfigError::ConfigTooLarge);
        }

        let mut contents = String::new();
        file.take(MAX_CONFIG_BYTES + 1)
            .read_to_string(&mut contents)
            .map_err(|source| ConfigError::Io {
                operation: "read",
                path: path.clone(),
                source,
            })?;
        if contents.len() as u64 > MAX_CONFIG_BYTES {
            return Err(ConfigError::ConfigTooLarge);
        }

        let mut document = toml::from_str::<toml::Table>(&contents)
            .map_err(|error| invalid_toml(&contents, &error))?;

        // A file from the future cannot be understood by guessing; say so before
        // strict deserialization buries it under unknown-field noise.
        let stored_version = document
            .get("schema_version")
            .and_then(toml::Value::as_integer)
            .and_then(|version| u32::try_from(version).ok())
            .unwrap_or(CURRENT_SCHEMA_VERSION);
        if stored_version > CURRENT_SCHEMA_VERSION {
            return Err(ConfigError::UnsupportedSchemaVersion {
                found: stored_version,
                supported: CURRENT_SCHEMA_VERSION,
            });
        }

        let migrated = migrate_to_current_schema(&mut document, stored_version);
        // Deserializing the original text keeps the parser's source spans, so a
        // rejection can name the offending line. A migrated document has been
        // rewritten in memory and no longer maps onto the file, so pointing at a
        // line there would be a guess.
        let config: AppConfig = if migrated {
            document
                .try_into()
                .map_err(|error: toml::de::Error| ConfigError::InvalidToml {
                    location: "the reprodb configuration".to_owned(),
                    detail: error.message().to_owned(),
                })?
        } else {
            toml::from_str(&contents).map_err(|error| invalid_toml(&contents, &error))?
        };
        config.validate()?;

        // Persisting is a convenience, not a precondition: a read-only config
        // directory must not break a command that only needed to read.
        if migrated {
            let _ = self.save(&config);
        }
        Ok(config)
    }

    /// Replaces an unreadable configuration with a fresh one, keeping the
    /// original as a timestamped backup.
    ///
    /// Recovery from a file we cannot parse necessarily discards what it held,
    /// so the caller must have asked for this explicitly and the old bytes are
    /// always preserved next to the new file.
    pub fn reset(&self) -> Result<PathBuf, ConfigError> {
        self.prepare_config_directory()?;
        let path = self.paths.config_file();
        let backup = path.with_extension(format!(
            "toml.bak-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs())
        ));
        match fs::rename(&path, &backup) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(ConfigError::Io {
                    operation: "back up",
                    path,
                    source,
                });
            }
        }
        self.save(&AppConfig::default())?;
        Ok(backup)
    }

    pub fn save(&self, config: &AppConfig) -> Result<(), ConfigError> {
        config.validate()?;
        self.prepare_config_directory()?;

        let lock_path = self.paths.config_lock_file();
        let lock_file = open_private_lock_file(&lock_path).map_err(|source| ConfigError::Io {
            operation: "open the write lock for",
            path: lock_path.clone(),
            source,
        })?;

        match fs4::FileExt::try_lock(&lock_file) {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(ConfigError::WriteLocked),
            Err(TryLockError::Error(source)) => {
                return Err(ConfigError::Io {
                    operation: "lock",
                    path: lock_path,
                    source,
                });
            }
        }

        self.save_while_locked(config)
    }

    fn prepare_config_directory(&self) -> Result<(), ConfigError> {
        let path = self.paths.config_dir();
        fs::create_dir_all(path).map_err(|source| ConfigError::Io {
            operation: "create the directory for",
            path: path.to_owned(),
            source,
        })?;
        set_private_directory_permissions(path).map_err(|source| ConfigError::Io {
            operation: "set directory permissions for",
            path: path.to_owned(),
            source,
        })
    }

    fn save_while_locked(&self, config: &AppConfig) -> Result<(), ConfigError> {
        let serialized = toml::to_string_pretty(config).map_err(|_| ConfigError::Serialization)?;
        let config_dir = self.paths.config_dir();
        let config_path = self.paths.config_file();
        let mut temporary =
            NamedTempFile::new_in(config_dir).map_err(|source| ConfigError::Io {
                operation: "create a temporary file for",
                path: config_path.clone(),
                source,
            })?;

        set_private_file_permissions(temporary.as_file()).map_err(|source| ConfigError::Io {
            operation: "set file permissions for",
            path: config_path.clone(),
            source,
        })?;
        temporary
            .write_all(serialized.as_bytes())
            .and_then(|_| temporary.flush())
            .and_then(|_| temporary.as_file().sync_all())
            .map_err(|source| ConfigError::Io {
                operation: "write",
                path: config_path.clone(),
                source,
            })?;

        let persisted = temporary
            .persist(&config_path)
            .map_err(|error| ConfigError::Io {
                operation: "replace",
                path: config_path.clone(),
                source: error.error,
            })?;
        persisted.sync_all().map_err(|source| ConfigError::Io {
            operation: "sync",
            path: config_path,
            source,
        })?;
        sync_directory(config_dir)
    }
}

fn validate_credential_scope(
    field: &'static str,
    key: CredentialKey,
    expected: CredentialScope,
) -> Result<(), ConfigError> {
    if key.scope() != expected {
        return Err(ConfigError::InvalidField {
            field,
            reason: match expected {
                CredentialScope::Source => "must use a source credential key",
                CredentialScope::Target => "must use a target credential key",
            },
        });
    }
    Ok(())
}

fn validate_plain_text(
    field: &'static str,
    value: &str,
    max_chars: usize,
) -> Result<(), ConfigError> {
    if value.is_empty() || value.trim() != value {
        return Err(ConfigError::InvalidField {
            field,
            reason: "cannot be empty or have surrounding whitespace",
        });
    }
    if value.chars().count() > max_chars {
        return Err(ConfigError::InvalidField {
            field,
            reason: "is longer than the supported limit",
        });
    }
    if value.chars().any(char::is_control) {
        return Err(ConfigError::InvalidField {
            field,
            reason: "cannot contain control characters",
        });
    }
    Ok(())
}

fn validate_docker_context(field: &'static str, value: &str) -> Result<(), ConfigError> {
    let valid = !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'));
    if !valid {
        return Err(ConfigError::InvalidField {
            field,
            reason: "must contain only ASCII letters, digits, `_`, `.` or `-`",
        });
    }
    Ok(())
}

fn validate_mysql_series(series: &str) -> Result<(), ConfigError> {
    validate_plain_text("profile.mysql_series", series, 16)?;
    let mut components = series.split('.');
    let major = components.next();
    let minor = components.next();
    let valid = components.next().is_none()
        && [major, minor].into_iter().all(|component| {
            component.is_some_and(|value| {
                !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
            })
        });
    if !valid {
        return Err(ConfigError::InvalidField {
            field: "profile.mysql_series",
            reason: "must contain a numeric major and minor version such as `8.4`",
        });
    }
    Ok(())
}

fn open_private_lock_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    set_private_file_permissions(&file)?;
    Ok(file)
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), ConfigError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| ConfigError::Io {
            operation: "sync the directory for",
            path: path.to_owned(),
            source,
        })
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), ConfigError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn test_repository(temp: &TempDir) -> ConfigRepository {
        ConfigRepository::new(AppPaths::new(
            temp.path().join("config"),
            temp.path().join("cache"),
            temp.path().join("data"),
        ))
    }

    fn valid_config() -> AppConfig {
        let profile_name = ProfileName::try_from("local-source").unwrap();
        let mut profiles = BTreeMap::new();
        profiles.insert(
            profile_name.clone(),
            SourceProfileConfig {
                host: "127.0.0.1".to_owned(),
                port: 3306,
                username: "root".to_owned(),
                credential_key: "source:550e8400-e29b-41d4-a716-446655440000"
                    .parse()
                    .unwrap(),
                mysql_family: MysqlFamily::Mysql,
                mysql_series: "8.4".to_owned(),
                production: false,
                tls_mode: MysqlTlsMode::Preferred,
                tls_material: Default::default(),
                client: MysqlClientConfig {
                    image: concat!(
                        "mysql:8.4.4@sha256:",
                        "1d967fb75a64dc3c2894c69285becfc2304ae0c3c4f4c715c297f3c12d60b01c"
                    )
                    .to_owned(),
                },
            },
        );

        AppConfig {
            active_profile: Some(profile_name),
            local_target: Some(LocalTargetConfig {
                docker_context: "desktop-linux".to_owned(),
                container_name: ContainerName::try_from("mysql-8").unwrap(),
                container_id: ContainerId::try_from("a".repeat(64)).unwrap(),
                username: "root".to_owned(),
                credential_key: "target:550e8400-e29b-41d4-a716-446655440001"
                    .parse()
                    .unwrap(),
                trust: LocalTargetTrust::UserConfirmed,
            }),
            profiles,
            ..AppConfig::default()
        }
    }

    #[test]
    fn source_fingerprint_tracks_connection_and_resolution_but_not_credentials() {
        let config = valid_config();
        let name = config.active_profile.as_ref().unwrap();
        let profile = config.profiles.get(name).unwrap();
        let original = source_profile_fingerprint(name, profile);

        let mut credential_changed = profile.clone();
        credential_changed.credential_key = CredentialKey::new(CredentialScope::Source);
        assert_eq!(
            source_profile_fingerprint(name, &credential_changed),
            original
        );

        let mut host_changed = profile.clone();
        host_changed.host = "db.internal".to_owned();
        assert_ne!(source_profile_fingerprint(name, &host_changed), original);
    }

    #[test]
    fn first_load_returns_an_empty_current_config_without_writing() {
        let temp = TempDir::new().unwrap();
        let repository = test_repository(&temp);

        assert_eq!(repository.load().unwrap(), AppConfig::default());
        assert!(!repository.paths().config_dir().exists());
    }

    #[test]
    fn saves_and_loads_a_strict_roundtrip() {
        let temp = TempDir::new().unwrap();
        let repository = test_repository(&temp);
        let expected = valid_config();

        repository.save(&expected).unwrap();

        assert_eq!(repository.load().unwrap(), expected);
        let contents = fs::read_to_string(repository.paths().config_file()).unwrap();
        assert!(contents.contains("schema_version = 2"));
        assert!(!contents.contains("password"));
    }

    #[test]
    fn saves_multiple_targets_without_duplicating_the_default() {
        let temp = TempDir::new().unwrap();
        let repository = test_repository(&temp);
        let mut expected = valid_config();
        let mut additional = expected.local_target.as_ref().unwrap().clone();
        additional.container_name = ContainerName::try_from("mysql-target").unwrap();
        additional.container_id = ContainerId::try_from("b".repeat(64)).unwrap();
        additional.credential_key = CredentialKey::new(CredentialScope::Target);
        expected
            .local_targets
            .insert(additional.container_name.clone(), additional);

        repository.save(&expected).unwrap();
        let loaded = repository.load().unwrap();
        let choices = loaded.configured_local_targets().collect::<Vec<_>>();

        assert_eq!(loaded, expected);
        assert_eq!(choices.len(), 2);
        assert!(choices[0].1);
        assert_eq!(
            loaded
                .local_target_named(&ContainerName::try_from("mysql-target").unwrap())
                .unwrap()
                .container_id
                .as_str(),
            "b".repeat(64)
        );
    }

    #[test]
    fn rejects_a_target_duplicated_or_misnamed_in_the_registry() {
        let temp = TempDir::new().unwrap();
        let repository = test_repository(&temp);
        let mut duplicated = valid_config();
        let active = duplicated.local_target.as_ref().unwrap().clone();
        duplicated
            .local_targets
            .insert(active.container_name.clone(), active);
        assert!(matches!(
            repository.save(&duplicated),
            Err(ConfigError::InvalidField {
                field: "local_targets",
                ..
            })
        ));

        let mut misnamed = valid_config();
        let mut target = misnamed.local_target.as_ref().unwrap().clone();
        target.container_name = ContainerName::try_from("mysql-target").unwrap();
        target.container_id = ContainerId::try_from("b".repeat(64)).unwrap();
        misnamed
            .local_targets
            .insert(ContainerName::try_from("mysql-other").unwrap(), target);
        assert!(repository.save(&misnamed).is_err());
    }

    /// The exact shape written by builds before the tenant model was dropped.
    const SCHEMA_V1_WITH_TENANT_MODEL: &str = r#"schema_version = 1
active_profile = "production"

[client_runtime]
type = "docker"
docker_context = "desktop-linux"

[local_target]
docker_context = "desktop-linux"
container_name = "mysql-8"
container_id = "62a9bfe47f1c1124d518f17eddf5c32b4ebfb4c8aa8f3af78bd61d554d3b599f"
username = "root"
credential_key = "target:74e31bf9-2604-41b1-95ff-534514f24eab"
central_database = "salt_central"
trust = "user-confirmed"

[profiles.production]
host = "db.example.com"
port = 3306
username = "admin"
credential_key = "source:869b6b57-1b09-42d1-8731-ccd773912404"
mysql_family = "mysql"
mysql_series = "8.4"
production = false
tls_mode = "preferred"

[profiles.production.client]
image = "mysql:8.4.4@sha256:1d967fb75a64dc3c2894c69285becfc2304ae0c3c4f4c715c297f3c12d60b01c"

[profiles.production.tenant_resolver]
type = "pattern"
pattern = "{tenant}"
"#;

    #[test]
    fn a_schema_1_config_is_migrated_instead_of_rejected() {
        let temp = TempDir::new().unwrap();
        let repository = test_repository(&temp);
        fs::create_dir_all(repository.paths().config_dir()).unwrap();
        fs::write(
            repository.paths().config_file(),
            SCHEMA_V1_WITH_TENANT_MODEL,
        )
        .unwrap();

        let config = repository.load().unwrap();

        // What the tenant model described is gone; everything a developer
        // configured survives.
        assert_eq!(config.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(
            config.active_profile,
            Some(ProfileName::try_from("production").unwrap())
        );
        assert_eq!(config.profiles.len(), 1);
        assert_eq!(
            config
                .local_target
                .as_ref()
                .unwrap()
                .container_name
                .as_str(),
            "mysql-8"
        );
        assert_eq!(
            config.profiles[&ProfileName::try_from("production").unwrap()].host,
            "db.example.com".to_owned()
        );
    }

    #[test]
    fn a_migrated_config_is_written_back_and_loads_again_unchanged() {
        let temp = TempDir::new().unwrap();
        let repository = test_repository(&temp);
        fs::create_dir_all(repository.paths().config_dir()).unwrap();
        fs::write(
            repository.paths().config_file(),
            SCHEMA_V1_WITH_TENANT_MODEL,
        )
        .unwrap();

        let first = repository.load().unwrap();
        let contents = fs::read_to_string(repository.paths().config_file()).unwrap();

        assert!(contents.contains("schema_version = 2"));
        assert!(!contents.contains("tenant_resolver"));
        assert!(!contents.contains("central_database"));
        assert_eq!(repository.load().unwrap(), first);
    }

    #[test]
    fn migrating_an_already_current_document_changes_nothing() {
        let mut document = toml::from_str::<toml::Table>(
            "schema_version = 2\n\n[profiles.local]\nhost = \"127.0.0.1\"\n",
        )
        .unwrap();
        let before = document.clone();

        assert!(!migrate_to_current_schema(
            &mut document,
            CURRENT_SCHEMA_VERSION
        ));
        assert_eq!(document, before);
    }

    #[test]
    fn the_rejection_names_the_field_and_its_line() {
        let temp = TempDir::new().unwrap();
        let repository = test_repository(&temp);
        fs::create_dir_all(repository.paths().config_dir()).unwrap();
        fs::write(
            repository.paths().config_file(),
            "schema_version = 2\n\n[local_target]\nnot_a_field = 1\n",
        )
        .unwrap();

        let error = repository.load().unwrap_err();

        let rendered = error.to_string();
        assert!(rendered.contains("not_a_field"), "{rendered}");
        assert!(rendered.contains("line 4"), "{rendered}");
    }

    #[test]
    fn resetting_keeps_the_unreadable_file_as_a_backup() {
        let temp = TempDir::new().unwrap();
        let repository = test_repository(&temp);
        fs::create_dir_all(repository.paths().config_dir()).unwrap();
        fs::write(repository.paths().config_file(), "this is not ( toml").unwrap();
        assert!(repository.load().is_err());

        let backup = repository.reset().unwrap();

        assert_eq!(
            fs::read_to_string(&backup).unwrap(),
            "this is not ( toml".to_owned()
        );
        assert_eq!(repository.load().unwrap(), AppConfig::default());
    }

    #[test]
    fn rejects_invalid_toml_without_echoing_its_contents() {
        let temp = TempDir::new().unwrap();
        let repository = test_repository(&temp);
        fs::create_dir_all(repository.paths().config_dir()).unwrap();
        let marker = "sensitive-marker";
        fs::write(
            repository.paths().config_file(),
            format!("schema_version = [\"{marker}\""),
        )
        .unwrap();

        let error = repository.load().unwrap_err();
        assert!(matches!(error, ConfigError::InvalidToml { .. }));
        assert!(!error.to_string().contains(marker));
        assert!(!format!("{error:?}").contains(marker));
    }

    #[test]
    fn rejects_unknown_fields_including_plaintext_passwords() {
        let temp = TempDir::new().unwrap();
        let repository = test_repository(&temp);
        fs::create_dir_all(repository.paths().config_dir()).unwrap();
        let marker = "a-password-that-must-not-leak";
        fs::write(
            repository.paths().config_file(),
            format!("schema_version = 1\npassword = \"{marker}\"\n"),
        )
        .unwrap();

        let error = repository.load().unwrap_err();
        assert!(matches!(error, ConfigError::InvalidToml { .. }));
        assert!(!error.to_string().contains(marker));
        assert!(!format!("{error:?}").contains(marker));
    }

    #[test]
    fn rejects_unsupported_schema_and_dangling_active_profile() {
        let temp = TempDir::new().unwrap();
        let repository = test_repository(&temp);
        fs::create_dir_all(repository.paths().config_dir()).unwrap();
        fs::write(repository.paths().config_file(), "schema_version = 3\n").unwrap();
        assert!(matches!(
            repository.load(),
            Err(ConfigError::UnsupportedSchemaVersion { found: 3, .. })
        ));

        fs::write(
            repository.paths().config_file(),
            "schema_version = 1\nactive_profile = \"missing\"\n",
        )
        .unwrap();
        assert!(matches!(
            repository.load(),
            Err(ConfigError::ActiveProfileNotFound)
        ));
    }

    #[test]
    fn rejects_credentials_with_the_wrong_scope() {
        let temp = TempDir::new().unwrap();
        let repository = test_repository(&temp);
        let mut config = valid_config();
        config.local_target.as_mut().unwrap().credential_key =
            "source:550e8400-e29b-41d4-a716-446655440001"
                .parse()
                .unwrap();

        let error = repository.save(&config).unwrap_err();
        assert!(matches!(
            error,
            ConfigError::InvalidField {
                field: "local_target.credential_key",
                ..
            }
        ));
    }

    #[test]
    fn rejects_a_client_outside_the_approved_catalog() {
        let temp = TempDir::new().unwrap();
        let repository = test_repository(&temp);
        let mut config = valid_config();
        config.profiles.values_mut().next().unwrap().client.image = "mysql:8".to_owned();

        let error = repository.save(&config).unwrap_err();
        assert!(matches!(
            error,
            ConfigError::InvalidField {
                field: "profile.client.image",
                ..
            }
        ));
    }

    #[test]
    fn production_profile_must_verify_the_ca_and_hostname() {
        let temp = TempDir::new().unwrap();
        let repository = test_repository(&temp);
        let mut config = valid_config();
        let profile = config.profiles.values_mut().next().unwrap();
        profile.production = true;
        profile.tls_mode = MysqlTlsMode::Preferred;

        let error = repository.save(&config).unwrap_err();

        assert!(matches!(
            error,
            ConfigError::InvalidField {
                field: "profile.tls_mode",
                ..
            }
        ));
    }

    #[test]
    fn verified_tls_requires_an_absolute_ca_path() {
        let temp = TempDir::new().unwrap();
        let repository = test_repository(&temp);
        let mut config = valid_config();
        config.profiles.values_mut().next().unwrap().tls_mode = MysqlTlsMode::VerifyIdentity;

        let missing = repository.save(&config).unwrap_err();
        assert!(matches!(
            missing,
            ConfigError::InvalidField {
                field: "profile.tls_material.ca",
                ..
            }
        ));

        config.profiles.values_mut().next().unwrap().tls_material.ca =
            Some(PathBuf::from("relative-ca.pem"));
        let relative = repository.save(&config).unwrap_err();
        assert!(matches!(
            relative,
            ConfigError::InvalidField {
                field: "profile.tls_material",
                ..
            }
        ));
    }

    #[test]
    fn client_tls_certificate_and_key_must_be_configured_together() {
        let temp = TempDir::new().unwrap();
        let repository = test_repository(&temp);
        let mut config = valid_config();
        config
            .profiles
            .values_mut()
            .next()
            .unwrap()
            .tls_material
            .cert = Some(PathBuf::from("/tmp/client.pem"));

        let error = repository.save(&config).unwrap_err();
        assert!(matches!(
            error,
            ConfigError::InvalidField {
                field: "profile.tls_material",
                ..
            }
        ));
    }

    #[test]
    fn local_target_and_client_runtime_must_share_a_docker_context() {
        let temp = TempDir::new().unwrap();
        let repository = test_repository(&temp);
        let mut config = valid_config();
        config.client_runtime.docker_context = Some("default".to_owned());

        let error = repository.save(&config).unwrap_err();

        assert!(matches!(
            error,
            ConfigError::InvalidField {
                field: "local_target.docker_context",
                ..
            }
        ));
    }

    #[test]
    fn local_target_rejects_an_untrusted_docker_context_without_a_runtime_context() {
        let temp = TempDir::new().unwrap();
        let repository = test_repository(&temp);
        let mut config = valid_config();
        config.local_target.as_mut().unwrap().docker_context = "default\n--host=remote".to_owned();

        let error = repository.save(&config).unwrap_err();

        assert!(matches!(
            error,
            ConfigError::InvalidField {
                field: "local_target.docker_context",
                ..
            }
        ));
    }

    #[test]
    fn fails_fast_when_another_writer_holds_the_lock() {
        let temp = TempDir::new().unwrap();
        let repository = test_repository(&temp);
        repository.prepare_config_directory().unwrap();
        let lock = open_private_lock_file(&repository.paths().config_lock_file()).unwrap();
        fs4::FileExt::try_lock(&lock).unwrap();

        assert!(matches!(
            repository.save(&valid_config()),
            Err(ConfigError::WriteLocked)
        ));
    }

    #[test]
    fn reports_write_failures_without_leaving_partial_files() {
        let temp = TempDir::new().unwrap();
        let config_dir = temp.path().join("not-a-directory");
        fs::write(&config_dir, "occupied").unwrap();
        let repository = ConfigRepository::new(AppPaths::new(
            &config_dir,
            temp.path().join("cache"),
            temp.path().join("data"),
        ));

        assert!(matches!(
            repository.save(&valid_config()),
            Err(ConfigError::Io { .. })
        ));
        assert_eq!(fs::read_to_string(config_dir).unwrap(), "occupied");
    }

    #[test]
    fn uses_the_injected_paths_without_platform_assumptions() {
        let paths = AppPaths::new("config-root", "cache-root", "data-root");

        assert_eq!(paths.config_dir(), Path::new("config-root"));
        assert_eq!(paths.cache_dir(), Path::new("cache-root"));
        assert_eq!(paths.data_dir(), Path::new("data-root"));
        assert_eq!(
            paths.config_file(),
            Path::new("config-root").join(CONFIG_FILE_NAME)
        );
    }

    #[test]
    fn reprodb_home_keeps_configuration_cache_and_data_under_one_root() {
        let paths = AppPaths::from_root("/users/developer/.reprodb");

        assert_eq!(
            paths.config_file(),
            Path::new("/users/developer/.reprodb/reprodb.toml")
        );
        assert_eq!(
            paths.config_lock_file(),
            Path::new("/users/developer/.reprodb/reprodb.lock")
        );
        assert_eq!(
            paths.cache_dir(),
            Path::new("/users/developer/.reprodb/cache")
        );
        assert_eq!(
            paths.data_dir(),
            Path::new("/users/developer/.reprodb/data")
        );
    }

    #[test]
    fn stores_configuration_with_private_unix_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        let repository = test_repository(&temp);
        repository.save(&valid_config()).unwrap();

        let directory_mode = fs::metadata(repository.paths().config_dir())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let file_mode = fs::metadata(repository.paths().config_file())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }
}
