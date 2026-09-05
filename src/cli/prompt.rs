use dialoguer::{Confirm, theme::SimpleTheme};
use thiserror::Error;

use crate::domain::ProfileName;

#[derive(Debug, Error)]
pub enum PromptError {
    #[error("could not read interactive input; run this command in a terminal or pass `--yes`")]
    Unavailable {
        #[source]
        source: dialoguer::Error,
    },
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
