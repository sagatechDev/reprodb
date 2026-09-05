use clap::{Args, Parser, Subcommand};

pub mod doctor;
pub mod output;
pub mod preview;
pub mod profile;
pub mod prompt;
pub mod setup;

pub use output::ColorChoice;

/// Local CLI for reproducing Salt tenant databases.
#[derive(Debug, Parser)]
#[command(name = "reprodb", version, about)]
pub struct Cli {
    /// Control colored output.
    #[arg(long, global = true, value_enum, default_value = "auto")]
    pub color: ColorChoice,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Configure the local Docker target.
    Setup(PreviewArgs),

    /// Manage MySQL source profiles.
    Profile(ProfileArgs),

    /// Validate configuration and local dependencies.
    Doctor(PreviewArgs),

    /// Create a managed cached dump without restoring it.
    Dump(TenantArgs),

    /// Restore a managed dump into the configured local target.
    Restore(RestoreArgs),

    /// Dump and restore a tenant into the configured local target.
    Pull(PullArgs),

    /// Inspect and clean the local dump cache.
    Cache(CacheArgs),
}

impl Commands {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Setup(_) => "setup",
            Self::Profile(_) => "profile",
            Self::Doctor(_) => "doctor",
            Self::Dump(_) => "dump",
            Self::Restore(_) => "restore",
            Self::Pull(_) => "pull",
            Self::Cache(_) => "cache",
        }
    }
}

#[derive(Debug, Args)]
pub struct ProfileArgs {
    #[command(subcommand)]
    pub command: ProfileCommands,
}

#[derive(Debug, Subcommand)]
pub enum ProfileCommands {
    /// Add a source profile through interactive prompts.
    Add(ProfileAddArgs),

    /// List configured source profiles.
    List,

    /// Select the active source profile.
    Use(ProfileNameArgs),

    /// Remove a source profile and its credential.
    Remove(ProfileRemoveArgs),
}

#[derive(Debug, Args)]
pub struct PreviewArgs {
    /// Show the planned interactive experience without making changes.
    #[arg(long)]
    pub preview: bool,
}

#[derive(Debug, Args)]
pub struct ProfileAddArgs {
    /// Profile name.
    #[arg(value_name = "NAME")]
    pub name: String,

    /// Show the planned interactive experience without making changes.
    #[arg(long)]
    pub preview: bool,
}

#[derive(Debug, Args)]
pub struct ProfileNameArgs {
    /// Profile name.
    #[arg(value_name = "NAME")]
    pub name: String,
}

#[derive(Debug, Args)]
pub struct ProfileRemoveArgs {
    /// Profile name.
    #[arg(value_name = "NAME")]
    pub name: String,

    /// Remove without an interactive confirmation.
    #[arg(long, short = 'y')]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct TenantArgs {
    /// Tenant ID or local domain alias.
    #[arg(value_name = "TENANT")]
    pub tenant: String,
}

#[derive(Debug, Args)]
pub struct RestoreArgs {
    /// Tenant ID or local domain alias.
    #[arg(value_name = "TENANT")]
    pub tenant: String,

    /// ID of a dump managed by reprodb.
    #[arg(long, value_name = "ID")]
    pub dump_id: String,
}

#[derive(Debug, Args)]
pub struct PullArgs {
    /// Tenant ID or local domain alias.
    #[arg(value_name = "TENANT")]
    pub tenant: String,

    /// Ignore a valid cache entry and create a fresh dump.
    #[arg(long)]
    pub fresh: bool,

    /// Show the planned flow without connecting, dumping or restoring.
    #[arg(long)]
    pub preview: bool,
}

#[derive(Debug, Args)]
pub struct CacheArgs {
    #[command(subcommand)]
    pub command: CacheCommands,
}

#[derive(Debug, Subcommand)]
pub enum CacheCommands {
    /// List complete dump artifacts.
    List,

    /// Remove expired and abandoned artifacts.
    Clean,

    /// Remove cached dumps for one tenant.
    Purge(TenantArgs),
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn parses_pull_with_fresh() {
        let cli = Cli::try_parse_from(["reprodb", "pull", "sagatec", "--fresh"]).unwrap();

        let Commands::Pull(arguments) = cli.command else {
            panic!("expected pull command");
        };
        assert_eq!(arguments.tenant, "sagatec");
        assert!(arguments.fresh);
        assert!(!arguments.preview);
        assert_eq!(cli.color, ColorChoice::Auto);
    }

    #[test]
    fn parses_a_global_color_choice_after_the_subcommand() {
        let cli = Cli::try_parse_from(["reprodb", "doctor", "--color", "always"]).unwrap();

        assert_eq!(cli.color, ColorChoice::Always);
    }

    #[test]
    fn parses_profile_add_preview() {
        let cli =
            Cli::try_parse_from(["reprodb", "profile", "add", "salt-local", "--preview"]).unwrap();

        let Commands::Profile(arguments) = cli.command else {
            panic!("expected profile command");
        };
        let ProfileCommands::Add(arguments) = arguments.command else {
            panic!("expected profile add command");
        };
        assert_eq!(arguments.name, "salt-local");
        assert!(arguments.preview);
    }

    #[test]
    fn parses_non_interactive_profile_removal() {
        let cli =
            Cli::try_parse_from(["reprodb", "profile", "remove", "salt-source", "--yes"]).unwrap();

        let Commands::Profile(arguments) = cli.command else {
            panic!("expected profile command");
        };
        let ProfileCommands::Remove(arguments) = arguments.command else {
            panic!("expected profile remove command");
        };
        assert_eq!(arguments.name, "salt-source");
        assert!(arguments.yes);
    }

    #[test]
    fn parses_restore_with_managed_dump_id() {
        let cli = Cli::try_parse_from(["reprodb", "restore", "sagatec", "--dump-id", "dump-123"])
            .unwrap();

        let Commands::Restore(arguments) = cli.command else {
            panic!("expected restore command");
        };
        assert_eq!(arguments.tenant, "sagatec");
        assert_eq!(arguments.dump_id, "dump-123");
    }
}
