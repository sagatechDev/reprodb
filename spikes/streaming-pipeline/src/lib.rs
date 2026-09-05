use std::{
    fs::{self, OpenOptions},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(all(unix, test))]
use std::os::unix::fs::PermissionsExt;

use secrecy::{ExposeSecret, SecretString};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum OptionFileError {
    #[error("{field} contains a NUL byte, which MySQL option files cannot represent safely")]
    InvalidValue { field: &'static str },

    #[error("option file already exists: {0}")]
    AlreadyExists(PathBuf),

    #[error("could not write the temporary MySQL option file: {0}")]
    Io(#[from] io::Error),
}

pub fn write_client_option_file(
    path: &Path,
    host: &str,
    port: u16,
    username: &str,
    password: &SecretString,
) -> Result<(), OptionFileError> {
    validate_value("host", host)?;
    validate_value("username", username)?;
    validate_value("password", password.expose_secret())?;

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);

    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(OptionFileError::AlreadyExists(path.to_path_buf()));
        }
        Err(error) => return Err(OptionFileError::Io(error)),
    };

    let result = write_contents(file, host, port, username, password);
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

fn write_contents(
    file: fs::File,
    host: &str,
    port: u16,
    username: &str,
    password: &SecretString,
) -> Result<(), OptionFileError> {
    let mut writer = BufWriter::new(file);
    writer.write_all(b"[client]\n")?;
    write_option(&mut writer, "host", host)?;
    writeln!(writer, "port={port}")?;
    write_option(&mut writer, "user", username)?;
    write_option(&mut writer, "password", password.expose_secret())?;
    writer.write_all(b"protocol=TCP\n")?;
    writer.flush()?;

    let file = writer.into_inner().map_err(|error| error.into_error())?;
    file.sync_all()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_mysql_option_file_metacharacters() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.cnf");
        let password = SecretString::from(" leading '\"#;\\\n\t\r\u{0008} trailing ");

        write_client_option_file(&path, "db.example.test", 3307, "user name", &password).unwrap();

        let rendered = fs::read_to_string(&path).unwrap();
        assert_eq!(
            rendered,
            concat!(
                "[client]\n",
                "host=\"db.example.test\"\n",
                "port=3307\n",
                "user=\"user name\"\n",
                "password=\" leading '\\\"#;\\\\\\n\\t\\r\\b trailing \"\n",
                "protocol=TCP\n",
            )
        );
    }

    #[cfg(unix)]
    #[test]
    fn creates_the_file_with_owner_only_permissions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.cnf");

        write_client_option_file(
            &path,
            "localhost",
            3306,
            "user",
            &SecretString::from("secret"),
        )
        .unwrap();

        let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn refuses_to_replace_an_existing_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.cnf");
        fs::write(&path, "keep me").unwrap();

        let error = write_client_option_file(
            &path,
            "localhost",
            3306,
            "user",
            &SecretString::from("secret"),
        )
        .unwrap_err();

        assert!(matches!(error, OptionFileError::AlreadyExists(_)));
        assert_eq!(fs::read_to_string(path).unwrap(), "keep me");
    }

    #[test]
    fn errors_and_debug_do_not_expose_the_password() {
        let password = SecretString::from("password-that-must-not-leak");
        assert!(!format!("{password:?}").contains(password.expose_secret()));

        let directory = tempfile::tempdir().unwrap();
        let error = write_client_option_file(
            &directory.path().join("client.cnf"),
            "localhost",
            3306,
            "user",
            &SecretString::from("invalid\0password-that-must-not-leak"),
        )
        .unwrap_err();

        assert!(!error.to_string().contains("password-that-must-not-leak"));
        assert!(!format!("{error:?}").contains("password-that-must-not-leak"));
    }
}
