use clap::Parser;
use reprodb::{Cli, execute_until_ctrl_c};
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("off")),
        )
        .with_writer(std::io::stderr)
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
