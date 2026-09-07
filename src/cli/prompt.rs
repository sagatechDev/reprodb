use std::io;

use dialoguer::{Confirm, Input, Select, theme::SimpleTheme};
use rpassword::{Config, ConfigBuilder, prompt_password_with_config};
use secrecy::SecretString;
use thiserror::Error;

use crate::{
    application::{NewLocalTargetInput, NewProfileInput, PullTargetChoice},
    cli::setup,
    domain::{ContainerName, DatabaseName, MysqlTlsMode, ProfileName},
    infrastructure::docker::DockerContainerCandidate,
};

#[derive(Debug, Error)]
pub enum PromptError {
    #[error("could not read interactive input; run this command in a terminal")]
    Unavailable {
        #[source]
        source: dialoguer::Error,
    },

    #[error("could not read the password from the terminal")]
    PasswordUnavailable {
        #[source]
        source: io::Error,
    },

    #[error("the local central database name is invalid")]
    InvalidCentralDatabase,

    #[error("the local restore database name is invalid")]
    InvalidRestoreDatabase,
}

pub fn collect_new_profile(name: ProfileName) -> Result<NewProfileInput, PromptError> {
    let theme = SimpleTheme;
    let host = Input::<String>::with_theme(&theme)
        .with_prompt("MySQL host")
        .default("127.0.0.1".to_owned())
        .validate_with(|value: &String| -> Result<(), &str> {
            if value.is_empty()
                || value.chars().count() > 255
                || value.chars().any(char::is_whitespace)
            {
                Err("enter a host without whitespace")
            } else {
                Ok(())
            }
        })
        .interact_text()
        .map_err(unavailable)?;
    let port = Input::<u16>::with_theme(&theme)
        .with_prompt("MySQL port")
        .default(3306)
        .validate_with(|value: &u16| -> Result<(), &str> {
            if *value == 0 {
                Err("port must be between 1 and 65535")
            } else {
                Ok(())
            }
        })
        .interact_text()
        .map_err(unavailable)?;
    let username = Input::<String>::with_theme(&theme)
        .with_prompt("MySQL username")
        .validate_with(|value: &String| -> Result<(), &str> {
            if value.is_empty()
                || value.trim() != value
                || value.chars().count() > 32
                || value.chars().any(char::is_control)
            {
                Err("enter a username up to 32 characters")
            } else {
                Ok(())
            }
        })
        .interact_text()
        .map_err(unavailable)?;
    let password = collect_password("MySQL password: ")?;
    let production = Confirm::with_theme(&theme)
        .with_prompt("Is this a production source?")
        .default(false)
        .interact()
        .map_err(unavailable)?;
    let tls_mode = if production {
        MysqlTlsMode::Required
    } else {
        let options = [
            "Required — fail unless the connection is encrypted (recommended)",
            "Preferred — use TLS when available, otherwise allow fallback",
            "Disabled — allow only an unencrypted connection",
        ];
        match Select::with_theme(&theme)
            .with_prompt("Connection security")
            .items(options)
            .default(0)
            .interact()
            .map_err(unavailable)?
        {
            0 => MysqlTlsMode::Required,
            1 => MysqlTlsMode::Preferred,
            2 => MysqlTlsMode::Disabled,
            _ => unreachable!("dialoguer returns an index from the provided options"),
        }
    };

    Ok(NewProfileInput {
        name,
        host,
        port,
        username,
        password,
        tls_mode,
        production,
    })
}

fn unavailable(source: dialoguer::Error) -> PromptError {
    PromptError::Unavailable { source }
}

fn collect_password(prompt: &str) -> Result<SecretString, PromptError> {
    loop {
        let config = ConfigBuilder::new().password_feedback_mask('*').build();
        let password = read_password_with_config(prompt, config)?;
        if !password.is_empty() {
            return Ok(SecretString::from(password));
        }

        eprintln!("  Password cannot be empty. Try again.");
    }
}

fn read_password_with_config(prompt: &str, config: Config) -> Result<String, PromptError> {
    prompt_password_with_config(prompt, config)
        .map_err(|source| PromptError::PasswordUnavailable { source })
}

pub fn select_local_target(candidates: &[DockerContainerCandidate]) -> Result<usize, PromptError> {
    Select::with_theme(&SimpleTheme)
        .with_prompt("Local restore target")
        .items(setup::choice_labels(candidates))
        .default(0)
        .interact()
        .map_err(unavailable)
}

pub fn collect_local_target(
    docker_context: String,
    candidate: &DockerContainerCandidate,
) -> Result<NewLocalTargetInput, PromptError> {
    let theme = SimpleTheme;
    let username = Input::<String>::with_theme(&theme)
        .with_prompt("Local MySQL username")
        .default("root".to_owned())
        .validate_with(|value: &String| -> Result<(), &str> {
            if value.is_empty()
                || value.trim() != value
                || value.chars().count() > 32
                || value.chars().any(char::is_control)
            {
                Err("enter a username up to 32 characters")
            } else {
                Ok(())
            }
        })
        .interact_text()
        .map_err(unavailable)?;
    let password = collect_password("Local MySQL password: ")?;
    let central_database = Input::<String>::with_theme(&theme)
        .with_prompt("Local central database")
        .default("salt_central".to_owned())
        .validate_with(|value: &String| -> Result<(), &str> {
            DatabaseName::try_from(value.as_str())
                .map(|_| ())
                .map_err(|_| "enter a safe non-administrative database name")
        })
        .interact_text()
        .map_err(unavailable)?;
    let tenant_database_prefix = Input::<String>::with_theme(&theme)
        .with_prompt("Tenant database prefix")
        .default("salt_".to_owned())
        .validate_with(|value: &String| -> Result<(), &str> {
            if value.is_empty()
                || value.len() > 32
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            {
                Err("enter 1-32 ASCII letters, digits or `_`")
            } else {
                Ok(())
            }
        })
        .interact_text()
        .map_err(unavailable)?;

    Ok(NewLocalTargetInput {
        docker_context,
        container_name: candidate.name.clone(),
        container_id: candidate.id.clone(),
        username,
        password,
        central_database: DatabaseName::try_from(central_database)
            .map_err(|_| PromptError::InvalidCentralDatabase)?,
        tenant_database_prefix,
        managed_by_reprodb: candidate.managed_by_reprodb,
    })
}

pub fn confirm_target_replacement(
    candidate: &DockerContainerCandidate,
) -> Result<bool, PromptError> {
    Confirm::with_theme(&SimpleTheme)
        .with_prompt(format!(
            "Reconfigure the saved target `{}` and replace its credential?",
            candidate.name
        ))
        .default(false)
        .interact()
        .map_err(unavailable)
}

pub fn confirm_container_start(candidate: &DockerContainerCandidate) -> Result<bool, PromptError> {
    Confirm::with_theme(&SimpleTheme)
        .with_prompt(format!(
            "Container `{}` is stopped. Start it now?",
            candidate.name
        ))
        .default(true)
        .interact()
        .map_err(unavailable)
}

pub fn confirm_profile_removal(name: &ProfileName) -> Result<bool, PromptError> {
    Confirm::with_theme(&SimpleTheme)
        .with_prompt(format!(
            "Remove source profile `{name}` and its stored credential?"
        ))
        .default(false)
        .interact()
        .map_err(|source| PromptError::Unavailable { source })
}

pub fn select_pull_database(default: &DatabaseName) -> Result<DatabaseName, PromptError> {
    let value = Input::<String>::with_theme(&SimpleTheme)
        .with_prompt("Local restore database")
        .default(default.as_str().to_owned())
        .validate_with(|value: &String| -> Result<(), &str> {
            DatabaseName::try_from(value.as_str())
                .map(|_| ())
                .map_err(|_| "enter a safe non-administrative database name")
        })
        .interact_text()
        .map_err(unavailable)?;
    DatabaseName::try_from(value).map_err(|_| PromptError::InvalidRestoreDatabase)
}

pub fn select_pull_target(choices: &[PullTargetChoice]) -> Result<ContainerName, PromptError> {
    let labels = choices
        .iter()
        .map(|choice| {
            if choice.is_default {
                format!("{} (default)", choice.container)
            } else {
                choice.container.to_string()
            }
        })
        .collect::<Vec<_>>();
    let default = choices
        .iter()
        .position(|choice| choice.is_default)
        .unwrap_or(0);
    let selected = Select::with_theme(&SimpleTheme)
        .with_prompt("Local restore container")
        .items(&labels)
        .default(default)
        .interact()
        .map_err(unavailable)?;
    Ok(choices[selected].container.clone())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn pasted_password_is_read_as_one_secret_without_echoing_its_contents() {
        let output = tempfile::NamedTempFile::new().unwrap();
        let output_path = output.path().to_string_lossy().into_owned();
        let config = ConfigBuilder::new()
            .input_data("pasted password !@# á\n")
            .output_file_path(&output_path)
            .password_feedback_mask('*')
            .build();

        let password = read_password_with_config("MySQL password: ", config).unwrap();
        let rendered = fs::read_to_string(output.path()).unwrap();

        assert_eq!(password, "pasted password !@# á");
        assert!(rendered.contains("MySQL password:"));
        assert!(!rendered.contains(&password));
    }

    #[test]
    fn password_prompt_errors_never_include_input_contents() {
        let config = ConfigBuilder::new()
            .input_file_path("/path/that/does/not/exist/reprodb-password")
            .output_discard()
            .password_feedback_mask('*')
            .build();

        let error = read_password_with_config("MySQL password: ", config).unwrap_err();

        assert_eq!(
            error.to_string(),
            "could not read the password from the terminal"
        );
    }
}
