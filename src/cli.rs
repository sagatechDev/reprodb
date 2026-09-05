use clap::Parser;

/// Reproduce a Salt tenant database in a local MySQL container.
#[derive(Debug, Parser)]
#[command(name = "reprodb", version, about)]
pub struct Cli {}
