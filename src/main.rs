use clap::Parser;
use reprodb::{Cli, execute_until_ctrl_c};
use std::{
    io::{self, IsTerminal},
    process::ExitCode,
};

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("off")),
        )
        .with_writer(io::stderr)
        .with_ansi(diagnostic_colors_enabled())
        .init();

    let cli = Cli::parse();

    match execute_until_ctrl_c(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if error.should_render_on_stderr() {
                eprintln!("{error}");
            }
            ExitCode::from(error.exit_code())
        }
    }
}

/// Diagnostic logs follow the same color policy as the CLI output itself, so a
/// piped or redirected stderr never receives escape sequences.
fn diagnostic_colors_enabled() -> bool {
    io::stderr().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && !std::env::var_os("TERM").is_some_and(|term| term == "dumb")
}
