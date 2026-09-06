use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use directories::{BaseDirs, ProjectDirs};
use fs4::TryLockError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::domain::{
    ContainerId, ContainerName, CredentialKey, CredentialScope, DatabaseName, MysqlTlsMode,
    PatternTenantResolver, ProfileName, Sha256Digest,
};
use crate::infrastructure::mysql::{ClientCatalog, ClientCatalogError};

pub const CURRENT_SCHEMA_VERSION: u32 = 1;
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
    legacy_config_file: Option<PathBuf>,
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
        let mut paths = Self::from_root(base.home_dir().join(REPRODB_HOME_DIRECTORY));
        paths.legacy_config_file = ProjectDirs::from("com", "Sagatech", "reprodb")
            .map(|directories| directories.config_dir().join(CONFIG_FILE_NAME));
        Ok(paths)
    }

    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            config_dir: root.clone(),
            cache_dir: root.join("cache"),
            data_dir: root.join("data"),
            legacy_config_file: None,
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
            legacy_config_file: None,
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

    fn config_file_for_read(&self) -> PathBuf {
        let current = self.config_file();
        if current.exists() {
            return current;
        }
        self.legacy_config_file
            .as_ref()
            .filter(|path| path.is_file())
            .cloned()
            .unwrap_or(current)
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

        if let Some(context) = &self.client_runtime.docker_context {
            validate_docker_context("client_runtime.docker_context", context)?;
        }

        for profile in self.profiles.values() {
            profile.validate()?;
        }

        Ok(())
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
    pub central_database: DatabaseName,
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
    pub client: MysqlClientConfig,
    pub tenant_resolver: TenantResolverConfig,
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
        if self.production && self.tls_mode != MysqlTlsMode::Required {
            return Err(ConfigError::InvalidField {
                field: "profile.tls_mode",
                reason: "must be `required` for a production source",
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
        self.tenant_resolver.validate()
    }
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
    match &profile.tenant_resolver {
        TenantResolverConfig::SaltCentral {
            central_database,
            allow_domain_lookup,
        } => {
            hash_field(&mut hasher, b"salt-central");
            hash_field(&mut hasher, central_database.as_str().as_bytes());
            hash_field(&mut hasher, &[u8::from(*allow_domain_lookup)]);
        }
        TenantResolverConfig::Pattern { pattern } => {
            hash_field(&mut hasher, b"pattern");
            hash_field(&mut hasher, pattern.as_bytes());
        }
    }
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum TenantResolverConfig {
    SaltCentral {
        central_database: DatabaseName,
        #[serde(default)]
        allow_domain_lookup: bool,
    },
    Pattern {
        pattern: String,
    },
}

impl TenantResolverConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        match self {
            Self::SaltCentral { .. } => Ok(()),
            Self::Pattern { pattern } => {
                validate_plain_text("profile.tenant_resolver.pattern", pattern, 128)?;
                PatternTenantResolver::new(pattern).map_err(|_| ConfigError::InvalidField {
                    field: "profile.tenant_resolver.pattern",
                    reason: "must contain exactly one `{tenant}` and only database-safe literals",
                })?;
                Ok(())
            }
        }
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

    #[error("the reprodb configuration is not valid TOML or contains unsupported fields")]
    InvalidToml,

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
        let path = self.paths.config_file_for_read();
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

        let config: AppConfig = toml::from_str(&contents).map_err(|_| ConfigError::InvalidToml)?;
        config.validate()?;
        Ok(config)
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
        let profile_name = ProfileName::try_from("salt-local").unwrap();
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
                client: MysqlClientConfig {
                    image: concat!(
                        "mysql:8.4.4@sha256:",
                        "1d967fb75a64dc3c2894c69285becfc2304ae0c3c4f4c715c297f3c12d60b01c"
                    )
                    .to_owned(),
                },
                tenant_resolver: TenantResolverConfig::SaltCentral {
                    central_database: DatabaseName::try_from("salt_central").unwrap(),
                    allow_domain_lookup: true,
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
                central_database: DatabaseName::try_from("salt_central").unwrap(),
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

        let mut resolver_changed = profile.clone();
        resolver_changed.tenant_resolver = TenantResolverConfig::Pattern {
            pattern: "{tenant}_data".to_owned(),
        };
        assert_ne!(
            source_profile_fingerprint(name, &resolver_changed),
            original
        );
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
        assert!(contents.contains("schema_version = 1"));
        assert!(!contents.contains("password"));
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
        assert!(matches!(error, ConfigError::InvalidToml));
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
        assert!(matches!(error, ConfigError::InvalidToml));
        assert!(!error.to_string().contains(marker));
        assert!(!format!("{error:?}").contains(marker));
    }

    #[test]
    fn rejects_unsupported_schema_and_dangling_active_profile() {
        let temp = TempDir::new().unwrap();
        let repository = test_repository(&temp);
        fs::create_dir_all(repository.paths().config_dir()).unwrap();
        fs::write(repository.paths().config_file(), "schema_version = 2\n").unwrap();
        assert!(matches!(
            repository.load(),
            Err(ConfigError::UnsupportedSchemaVersion { found: 2, .. })
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
    fn production_profile_must_require_tls() {
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
    fn validates_pattern_resolvers_before_persisting_configuration() {
        let temp = TempDir::new().unwrap();
        let repository = test_repository(&temp);
        let mut config = valid_config();
        config.profiles.values_mut().next().unwrap().tenant_resolver =
            TenantResolverConfig::Pattern {
                pattern: "salt_{tenant}_{tenant}".to_owned(),
            };

        let error = repository.save(&config).unwrap_err();

        assert!(matches!(
            error,
            ConfigError::InvalidField {
                field: "profile.tenant_resolver.pattern",
                ..
            }
        ));
        assert!(!repository.paths().config_file().exists());
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
    fn legacy_native_configuration_is_read_until_the_new_home_is_written() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join(".reprodb");
        let legacy = temp.path().join("legacy/reprodb.toml");
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, "schema_version = 1\n").unwrap();
        let mut paths = AppPaths::from_root(&root);
        paths.legacy_config_file = Some(legacy);
        let repository = ConfigRepository::new(paths);

        assert_eq!(repository.load().unwrap(), AppConfig::default());
        assert!(!repository.paths().config_file().exists());

        let config = valid_config();
        repository.save(&config).unwrap();
        assert_eq!(repository.load().unwrap(), config);
        assert!(repository.paths().config_file().exists());
    }

    #[cfg(unix)]
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
