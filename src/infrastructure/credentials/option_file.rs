use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
};

use secrecy::{ExposeSecret, SecretString, zeroize::Zeroize};
use tempfile::{Builder, TempDir};
use thiserror::Error;

use crate::domain::{MysqlTlsMaterialPaths, MysqlTlsMode};

pub const MYSQL_SECRETS_CONTAINER_DIRECTORY: &str = "/run/secrets/reprodb";
pub const MYSQL_OPTION_FILE_CONTAINER_PATH: &str = "/run/secrets/reprodb/client.cnf";
pub const MYSQL_CA_CONTAINER_PATH: &str = "/run/secrets/reprodb/ca.pem";
pub const MYSQL_CERT_CONTAINER_PATH: &str = "/run/secrets/reprodb/client-cert.pem";
pub const MYSQL_KEY_CONTAINER_PATH: &str = "/run/secrets/reprodb/client-key.pem";
const OPTION_FILE_NAME: &str = "client.cnf";
const MAX_TLS_MATERIAL_BYTES: u64 = 1024 * 1024;

pub struct MysqlOptionFile {
    directory: TempDir,
    path: PathBuf,
}

impl std::fmt::Debug for MysqlOptionFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MysqlOptionFile")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl MysqlOptionFile {
    pub fn create(
        host: &str,
        port: u16,
        username: &str,
        password: &SecretString,
        tls_mode: MysqlTlsMode,
    ) -> Result<Self, OptionFileError> {
        Self::create_with_tls_material(
            host,
            port,
            username,
            password,
            tls_mode,
            &MysqlTlsMaterialPaths::default(),
        )
    }

    pub fn create_with_tls_material(
        host: &str,
        port: u16,
        username: &str,
        password: &SecretString,
        tls_mode: MysqlTlsMode,
        tls_material: &MysqlTlsMaterialPaths,
    ) -> Result<Self, OptionFileError> {
        validate_value("host", host)?;
        validate_value("username", username)?;
        validate_value("password", password.expose_secret())?;

        let directory = Builder::new()
            .prefix("reprodb-mysql-")
            .tempdir()
            .map_err(|source| OptionFileError::Io {
                operation: "create the private temporary directory",
                source,
            })?;
        set_private_directory_permissions(directory.path()).map_err(|source| {
            OptionFileError::Io {
                operation: "restrict the temporary directory",
                source,
            }
        })?;
        let path = directory.path().join(OPTION_FILE_NAME);
        copy_tls_material(directory.path(), tls_material)?;
        let file = create_private_file(&path).map_err(|source| OptionFileError::Io {
            operation: "create the MySQL option file",
            source,
        })?;

        if let Err(error) =
            write_contents(file, host, port, username, password, tls_mode, tls_material)
        {
            let _ = fs::remove_file(&path);
            return Err(error);
        }

        Ok(Self { directory, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn directory_path(&self) -> &Path {
        self.directory.path()
    }

    pub const fn container_path(&self) -> &'static str {
        MYSQL_OPTION_FILE_CONTAINER_PATH
    }
}

#[derive(Debug, Error)]
pub enum OptionFileError {
    #[error("invalid MySQL option-file value for `{field}`")]
    InvalidValue { field: &'static str },

    #[error("invalid or unavailable MySQL TLS material for `{field}`")]
    InvalidTlsMaterial { field: &'static str },

    #[error("could not {operation}")]
    Io {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
}

fn create_private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    set_private_file_permissions(&file)?;
    Ok(file)
}

fn write_contents(
    file: File,
    host: &str,
    port: u16,
    username: &str,
    password: &SecretString,
    tls_mode: MysqlTlsMode,
    tls_material: &MysqlTlsMaterialPaths,
) -> Result<(), OptionFileError> {
    let mut writer = BufWriter::new(file);
    writer
        .write_all(b"[client]\n")
        .and_then(|_| write_option(&mut writer, "host", host))
        .and_then(|_| writeln!(writer, "port={port}"))
        .and_then(|_| write_option(&mut writer, "user", username))
        .and_then(|_| write_option(&mut writer, "password", password.expose_secret()))
        .and_then(|_| writer.write_all(b"protocol=TCP\n"))
        .and_then(|_| writeln!(writer, "ssl-mode={}", tls_mode.option_value()))
        .and_then(|_| write_tls_options(&mut writer, tls_material))
        .and_then(|_| writer.flush())
        .map_err(|source| OptionFileError::Io {
            operation: "write the MySQL option file",
            source,
        })?;

    let file = writer.into_inner().map_err(|error| OptionFileError::Io {
        operation: "finish writing the MySQL option file",
        source: error.into_error(),
    })?;
    file.sync_all().map_err(|source| OptionFileError::Io {
        operation: "sync the MySQL option file",
        source,
    })
}

fn write_tls_options(writer: &mut impl Write, material: &MysqlTlsMaterialPaths) -> io::Result<()> {
    if material.ca.is_some() {
        writeln!(writer, "ssl-ca={MYSQL_CA_CONTAINER_PATH}")?;
    }
    if material.cert.is_some() {
        writeln!(writer, "ssl-cert={MYSQL_CERT_CONTAINER_PATH}")?;
    }
    if material.key.is_some() {
        writeln!(writer, "ssl-key={MYSQL_KEY_CONTAINER_PATH}")?;
    }
    Ok(())
}

fn copy_tls_material(
    directory: &Path,
    material: &MysqlTlsMaterialPaths,
) -> Result<(), OptionFileError> {
    for (field, source, destination) in [
        ("ca", material.ca.as_deref(), "ca.pem"),
        ("cert", material.cert.as_deref(), "client-cert.pem"),
        ("key", material.key.as_deref(), "client-key.pem"),
    ] {
        let Some(source) = source else { continue };
        if !source.is_absolute() {
            return Err(OptionFileError::InvalidTlsMaterial { field });
        }
        let metadata =
            fs::metadata(source).map_err(|_| OptionFileError::InvalidTlsMaterial { field })?;
        if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_TLS_MATERIAL_BYTES {
            return Err(OptionFileError::InvalidTlsMaterial { field });
        }
        let mut bytes =
            fs::read(source).map_err(|_| OptionFileError::InvalidTlsMaterial { field })?;
        if bytes.len() as u64 > MAX_TLS_MATERIAL_BYTES {
            return Err(OptionFileError::InvalidTlsMaterial { field });
        }
        let destination = directory.join(destination);
        let mut output = create_private_file(&destination)
            .map_err(|_| OptionFileError::InvalidTlsMaterial { field })?;
        let result = output
            .write_all(&bytes)
            .and_then(|()| output.sync_all())
            .map_err(|_| OptionFileError::InvalidTlsMaterial { field });
        bytes.zeroize();
        result?;
    }
    Ok(())
}

fn write_option(writer: &mut impl Write, name: &str, value: &str) -> io::Result<()> {
    write!(writer, "{name}=\"")?;
    for character in value.chars() {
        match character {
            '\u{0008}' => writer.write_all(b"\\b")?,
            '\t' => writer.write_all(b"\\t")?,
            '\n' => writer.write_all(b"\\n")?,
            '\r' => writer.write_all(b"\\r")?,
            '\\' => writer.write_all(b"\\\\")?,
            '"' => writer.write_all(b"\\\"")?,
            character => write!(writer, "{character}")?,
        }
    }
    writer.write_all(b"\"\n")
}

fn validate_value(field: &'static str, value: &str) -> Result<(), OptionFileError> {
    if value.contains('\0') {
        return Err(OptionFileError::InvalidValue { field });
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_mysql_option_file_metacharacters() {
        let password = SecretString::from(" leading '\"#;\\\n\t\r\u{0008} trailing ");
        let option_file = MysqlOptionFile::create(
            "db.example.test",
            3307,
            "user name",
            &password,
            MysqlTlsMode::Required,
        )
        .unwrap();

        let rendered = fs::read_to_string(option_file.path()).unwrap();
        assert_eq!(
            rendered,
            concat!(
                "[client]\n",
                "host=\"db.example.test\"\n",
                "port=3307\n",
                "user=\"user name\"\n",
                "password=\" leading '\\\"#;\\\\\\n\\t\\r\\b trailing \"\n",
                "protocol=TCP\n",
                "ssl-mode=REQUIRED\n",
            )
        );
    }

    #[test]
    fn rejects_nul_without_exposing_the_secret() {
        let marker = "password-that-must-not-leak";
        let error = MysqlOptionFile::create(
            "localhost",
            3306,
            "root",
            &SecretString::from(format!("invalid\0{marker}")),
            MysqlTlsMode::Preferred,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            OptionFileError::InvalidValue { field: "password" }
        ));
        assert!(!error.to_string().contains(marker));
        assert!(!format!("{error:?}").contains(marker));
    }

    #[test]
    fn guard_removes_the_private_directory_and_file_on_drop() {
        let option_file = MysqlOptionFile::create(
            "localhost",
            3306,
            "root",
            &SecretString::from("secret"),
            MysqlTlsMode::Preferred,
        )
        .unwrap();
        let directory = option_file.directory_path().to_owned();
        let path = option_file.path().to_owned();
        assert!(path.exists());

        drop(option_file);

        assert!(!path.exists());
        assert!(!directory.exists());
    }

    #[test]
    fn exposes_only_the_fixed_container_destination() {
        let option_file = MysqlOptionFile::create(
            "localhost",
            3306,
            "root",
            &SecretString::from("secret"),
            MysqlTlsMode::Preferred,
        )
        .unwrap();

        assert_eq!(
            option_file.container_path(),
            MYSQL_OPTION_FILE_CONTAINER_PATH
        );
    }

    #[test]
    fn copies_tls_material_privately_and_references_only_fixed_container_paths() {
        let sources = TempDir::new().unwrap();
        let ca = sources.path().join("source-ca.pem");
        let cert = sources.path().join("source-cert.pem");
        let key = sources.path().join("source-key.pem");
        fs::write(&ca, b"test-ca").unwrap();
        fs::write(&cert, b"test-cert").unwrap();
        fs::write(&key, b"test-key").unwrap();

        let option_file = MysqlOptionFile::create_with_tls_material(
            "db.example.test",
            3306,
            "reprodb",
            &SecretString::from("secret"),
            MysqlTlsMode::VerifyIdentity,
            &MysqlTlsMaterialPaths {
                ca: Some(ca.clone()),
                cert: Some(cert.clone()),
                key: Some(key.clone()),
            },
        )
        .unwrap();

        let rendered = fs::read_to_string(option_file.path()).unwrap();
        assert!(rendered.contains(&format!("ssl-ca={MYSQL_CA_CONTAINER_PATH}")));
        assert!(rendered.contains(&format!("ssl-cert={MYSQL_CERT_CONTAINER_PATH}")));
        assert!(rendered.contains(&format!("ssl-key={MYSQL_KEY_CONTAINER_PATH}")));
        assert!(!rendered.contains(ca.to_string_lossy().as_ref()));
        assert!(!rendered.contains(cert.to_string_lossy().as_ref()));
        assert!(!rendered.contains(key.to_string_lossy().as_ref()));
        assert_eq!(
            fs::read(option_file.directory_path().join("ca.pem")).unwrap(),
            b"test-ca"
        );
        assert_eq!(
            fs::read(option_file.directory_path().join("client-cert.pem")).unwrap(),
            b"test-cert"
        );
        assert_eq!(
            fs::read(option_file.directory_path().join("client-key.pem")).unwrap(),
            b"test-key"
        );
    }

    #[test]
    fn invalid_tls_material_errors_do_not_echo_host_paths() {
        let marker = "private-certificate-marker";
        let error = MysqlOptionFile::create_with_tls_material(
            "db.example.test",
            3306,
            "reprodb",
            &SecretString::from("secret"),
            MysqlTlsMode::VerifyIdentity,
            &MysqlTlsMaterialPaths {
                ca: Some(PathBuf::from(marker)),
                cert: None,
                key: None,
            },
        )
        .unwrap_err();

        assert!(matches!(
            error,
            OptionFileError::InvalidTlsMaterial { field: "ca" }
        ));
        assert!(!error.to_string().contains(marker));
        assert!(!format!("{error:?}").contains(marker));
    }

    #[cfg(unix)]
    #[test]
    fn creates_private_directory_and_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let option_file = MysqlOptionFile::create(
            "localhost",
            3306,
            "root",
            &SecretString::from("secret"),
            MysqlTlsMode::Preferred,
        )
        .unwrap();
        let directory_mode = fs::metadata(option_file.directory_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let file_mode = fs::metadata(option_file.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(directory_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }
}
