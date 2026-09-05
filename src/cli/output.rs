use std::io::{self, IsTerminal};

use clap::ValueEnum;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum ColorChoice {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputStyle {
    colors_enabled: bool,
}

impl OutputStyle {
    pub fn stdout(choice: ColorChoice) -> Self {
        Self::resolve(
            choice,
            io::stdout().is_terminal(),
            std::env::var_os("NO_COLOR").is_some(),
            std::env::var_os("TERM").is_some_and(|term| term == "dumb"),
        )
    }

    pub const fn plain() -> Self {
        Self {
            colors_enabled: false,
        }
    }

    pub const fn colored() -> Self {
        Self {
            colors_enabled: true,
        }
    }

    const fn resolve(
        choice: ColorChoice,
        is_terminal: bool,
        no_color: bool,
        dumb_terminal: bool,
    ) -> Self {
        let colors_enabled = match choice {
            ColorChoice::Always => true,
            ColorChoice::Never => false,
            ColorChoice::Auto => is_terminal && !no_color && !dumb_terminal,
        };
        Self { colors_enabled }
    }

    pub fn brand(&self, value: &str) -> String {
        self.paint("1;36", value)
    }

    pub fn section(&self, value: &str) -> String {
        self.paint("1", value)
    }

    pub fn success(&self, value: &str) -> String {
        self.paint("1;32", value)
    }

    pub fn attention(&self, value: &str) -> String {
        self.paint("1;33", value)
    }

    pub fn selected(&self, value: &str) -> String {
        self.paint("1;36", value)
    }

    pub fn value(&self, value: &str) -> String {
        self.paint("1", value)
    }

    pub fn muted(&self, value: &str) -> String {
        self.paint("2", value)
    }

    fn paint(&self, ansi_code: &str, value: &str) -> String {
        if self.colors_enabled {
            format!("\u{1b}[{ansi_code}m{value}\u{1b}[0m")
        } else {
            value.to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_uses_color_only_on_a_capable_terminal() {
        assert_eq!(
            OutputStyle::resolve(ColorChoice::Auto, true, false, false),
            OutputStyle::colored()
        );
        assert_eq!(
            OutputStyle::resolve(ColorChoice::Auto, false, false, false),
            OutputStyle::plain()
        );
        assert_eq!(
            OutputStyle::resolve(ColorChoice::Auto, true, true, false),
            OutputStyle::plain()
        );
        assert_eq!(
            OutputStyle::resolve(ColorChoice::Auto, true, false, true),
            OutputStyle::plain()
        );
    }

    #[test]
    fn explicit_color_choice_wins_over_terminal_detection() {
        assert_eq!(
            OutputStyle::resolve(ColorChoice::Always, false, true, true),
            OutputStyle::colored()
        );
        assert_eq!(
            OutputStyle::resolve(ColorChoice::Never, true, false, false),
            OutputStyle::plain()
        );
    }

    #[test]
    fn status_remains_readable_without_ansi_codes() {
        assert_eq!(OutputStyle::plain().success("✓ connected"), "✓ connected");
        assert_eq!(
            OutputStyle::colored().success("✓ connected"),
            "\u{1b}[1;32m✓ connected\u{1b}[0m"
        );
    }
}
