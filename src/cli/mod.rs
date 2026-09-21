use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::domain::MysqlTlsMode;

pub mod cache;
pub mod database;
pub mod doctor;
pub mod dump;
pub mod output;
pub mod preview;
pub mod profile;
pub mod prompt;
pub mod pull;
pub mod restore;
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
    Setup(SetupArgs),

    /// Manage MySQL source profiles.
    Profile(ProfileArgs),

    /// Validate configuration and local dependencies.
    Doctor(PreviewArgs),

    /// Create a managed cached database dump without restoring it.
    Dump(DatabaseArgs),

    /// Restore a managed dump into the configured local target.
    Restore(RestoreArgs),

    /// Dump and restore a source database into the configured local target.
    Pull(PullArgs),

    /// Inspect and prune the local dump cache.
    Cache(CacheArgs),

    /// Inspect databases available in the active source profile.
    #[command(visible_alias = "db")]
    Database(DatabaseCommandArgs),
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
            Self::Database(_) => "database",
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
pub struct SetupArgs {
    /// Show the planned interactive experience without making changes.
    #[arg(long, conflicts_with = "non_interactive")]
    pub preview: bool,

    /// Start from an empty configuration, keeping the current file as a backup.
    /// Use when the stored configuration can no longer be read.
    #[arg(long, conflicts_with = "preview")]
    pub reset_config: bool,

    /// Configure using flags and a password read from stdin.
    #[arg(long)]
    pub non_interactive: bool,

    /// Exact running Docker container name.
    #[arg(long, value_name = "CONTAINER", requires = "non_interactive")]
    pub target: Option<String>,

    /// Local MySQL username.
    #[arg(long, default_value = "root", requires = "non_interactive")]
    pub username: String,

    /// Read exactly one password line from stdin.
    #[arg(long, requires = "non_interactive")]
    pub password_stdin: bool,

    /// Replace an existing target configuration without prompting.
    #[arg(long, requires = "non_interactive")]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct ProfileAddArgs {
    /// Profile name.
    #[arg(value_name = "NAME")]
    pub name: String,

    /// Show the planned interactive experience without making changes.
    #[arg(long)]
    pub preview: bool,

    /// Configure using flags and a password read from stdin.
    #[arg(long, conflicts_with = "preview")]
    pub non_interactive: bool,

    /// MySQL source host.
    #[arg(long, default_value = "127.0.0.1", requires = "non_interactive")]
    pub host: String,

    /// MySQL source port.
    #[arg(long, default_value_t = 3306, requires = "non_interactive")]
    pub port: u16,

    /// MySQL source username.
    #[arg(long, requires = "non_interactive")]
    pub username: Option<String>,

    /// Required source transport policy.
    #[arg(
        long,
        value_enum,
        default_value = "required",
        requires = "non_interactive"
    )]
    pub tls: CliTlsMode,

    /// Classify this source as production.
    #[arg(long, requires = "non_interactive")]
    pub production: bool,

    /// Read exactly one password line from stdin.
    #[arg(long, requires = "non_interactive")]
    pub password_stdin: bool,

    /// CA certificate path for verify-ca or verify-identity.
    #[arg(long, value_name = "PATH", requires = "non_interactive")]
    pub tls_ca: Option<std::path::PathBuf>,

    /// Optional client certificate path; requires --tls-key.
    #[arg(long, value_name = "PATH", requires_all = ["tls_key", "non_interactive"])]
    pub tls_cert: Option<std::path::PathBuf>,

    /// Optional client private-key path; requires --tls-cert.
    #[arg(long, value_name = "PATH", requires_all = ["tls_cert", "non_interactive"])]
    pub tls_key: Option<std::path::PathBuf>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum CliTlsMode {
    Disabled,
    Preferred,
    #[default]
    Required,
    VerifyCa,
    VerifyIdentity,
}

impl From<CliTlsMode> for MysqlTlsMode {
    fn from(value: CliTlsMode) -> Self {
        match value {
            CliTlsMode::Disabled => Self::Disabled,
            CliTlsMode::Preferred => Self::Preferred,
            CliTlsMode::Required => Self::Required,
            CliTlsMode::VerifyCa => Self::VerifyCa,
            CliTlsMode::VerifyIdentity => Self::VerifyIdentity,
        }
    }
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
pub struct DatabaseArgs {
    /// Database name on the active source profile.
    #[arg(value_name = "DATABASE")]
    pub database: String,
}

#[derive(Debug, Args)]
pub struct RestoreArgs {
    /// Source database name stored in the managed dump.
    #[arg(value_name = "DATABASE")]
    pub database: String,

    /// ID of a dump managed by reprodb. Omit to choose from the stored dumps.
    #[arg(long, value_name = "ID")]
    pub dump_id: Option<String>,

    /// Restore into this configured local MySQL container.
    #[arg(long, value_name = "CONTAINER")]
    pub target: Option<String>,

    /// Restore into this local database instead of the source database name.
    #[arg(long = "database", value_name = "DATABASE")]
    pub target_database: Option<String>,
}

#[derive(Debug, Args)]
pub struct PullArgs {
    /// Database name on the active source profile.
    #[arg(value_name = "DATABASE")]
    pub database: String,

    /// Restore into this configured local MySQL container.
    #[arg(long, value_name = "CONTAINER")]
    pub target: Option<String>,

    /// Restore into this local database instead of the source database name.
    #[arg(long = "database", value_name = "DATABASE")]
    pub target_database: Option<String>,

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

#[derive(Debug, Args)]
pub struct DatabaseCommandArgs {
    #[command(subcommand)]
    pub command: DatabaseCommands,
}

#[derive(Debug, Subcommand)]
pub enum DatabaseCommands {
    /// List non-administrative databases without changing the source.
    List(DatabaseListArgs),
}

#[derive(Debug, Args)]
pub struct DatabaseListArgs {
    /// Maximum number of databases to return.
    #[arg(long, default_value_t = crate::application::DEFAULT_DATABASE_LIST_LIMIT)]
    pub limit: u16,
}

#[derive(Debug, Subcommand)]
pub enum CacheCommands {
    /// List complete dump artifacts.
    List,

    /// Permanently remove managed dumps.
    Prune(PruneArgs),
}

#[derive(Debug, Args)]
pub struct PruneArgs {
    /// Limit the removal to one database, across every profile.
    #[arg(value_name = "DATABASE")]
    pub database: Option<String>,

    /// Remove dumps at least this old, for example `24h`, `3d` or `90m`.
    #[arg(long, value_name = "DURATION", conflicts_with_all = ["keep_last", "all"])]
    pub older_than: Option<String>,

    /// Keep the newest N dumps of every profile and database pair.
    #[arg(long, value_name = "N", conflicts_with_all = ["older_than", "all"])]
    pub keep_last: Option<usize>,

    /// Remove every dump in scope.
    #[arg(long, conflicts_with_all = ["older_than", "keep_last"])]
    pub all: bool,

    /// Skip the confirmation prompt.
    #[arg(long)]
    pub yes: bool,
}

/// Parses `90m`, `24h`, `3d` or a bare number of seconds.
pub fn parse_duration_seconds(input: &str) -> Result<u64, DurationParseError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(DurationParseError(input.to_owned()));
    }
    let (digits, multiplier) = match trimmed.as_bytes()[trimmed.len() - 1] {
        b's' => (&trimmed[..trimmed.len() - 1], 1),
        b'm' => (&trimmed[..trimmed.len() - 1], 60),
        b'h' => (&trimmed[..trimmed.len() - 1], 60 * 60),
        b'd' => (&trimmed[..trimmed.len() - 1], 24 * 60 * 60),
        _ => (trimmed, 1),
    };
    digits
        .parse::<u64>()
        .ok()
        .and_then(|value| value.checked_mul(multiplier))
        .ok_or_else(|| DurationParseError(input.to_owned()))
}

#[derive(Debug, thiserror::Error)]
#[error(
    "invalid duration `{0}`; use a number optionally suffixed with s, m, h or d, for example `24h`"
)]
pub struct DurationParseError(String);

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn parses_pull_with_fresh() {
        let cli = Cli::try_parse_from(["reprodb", "pull", "acme", "--fresh"]).unwrap();

        let Commands::Pull(arguments) = cli.command else {
            panic!("expected pull command");
        };
        assert_eq!(arguments.database, "acme");
        assert!(arguments.fresh);
        assert!(arguments.target.is_none());
        assert!(arguments.target_database.is_none());
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
        let cli = Cli::try_parse_from(["reprodb", "profile", "add", "local-source", "--preview"])
            .unwrap();

        let Commands::Profile(arguments) = cli.command else {
            panic!("expected profile command");
        };
        let ProfileCommands::Add(arguments) = arguments.command else {
            panic!("expected profile add command");
        };
        assert_eq!(arguments.name, "local-source");
        assert!(arguments.preview);
        assert!(!arguments.non_interactive);
    }

    #[test]
    fn parses_non_interactive_setup_and_profile_without_password_values() {
        let setup = Cli::try_parse_from([
            "reprodb",
            "setup",
            "--non-interactive",
            "--target",
            "mysql-target",
            "--password-stdin",
            "--yes",
        ])
        .unwrap();
        let Commands::Setup(setup) = setup.command else {
            panic!("expected setup command")
        };
        assert_eq!(setup.target.as_deref(), Some("mysql-target"));
        assert_eq!(setup.username, "root");
        assert!(setup.password_stdin);

        let profile = Cli::try_parse_from([
            "reprodb",
            "profile",
            "add",
            "ci-source",
            "--non-interactive",
            "--host",
            "127.0.0.1",
            "--port",
            "3307",
            "--username",
            "root",
            "--tls",
            "required",
            "--password-stdin",
        ])
        .unwrap();
        let Commands::Profile(profile) = profile.command else {
            panic!("expected profile command")
        };
        let ProfileCommands::Add(profile) = profile.command else {
            panic!("expected profile add")
        };
        assert!(profile.non_interactive);
        assert_eq!(profile.port, 3307);
        assert_eq!(profile.tls, CliTlsMode::Required);
        assert!(profile.password_stdin);
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
    fn parses_database_list_and_its_alias_with_a_bounded_limit() {
        let canonical =
            Cli::try_parse_from(["reprodb", "database", "list", "--limit", "25"]).unwrap();
        assert!(matches!(canonical.command, Commands::Database(_)));
        let cli = Cli::try_parse_from(["reprodb", "db", "list", "--limit", "25"]).unwrap();
        let Commands::Database(arguments) = cli.command else {
            panic!("expected database command");
        };
        let DatabaseCommands::List(arguments) = arguments.command;
        assert_eq!(arguments.limit, 25);
    }

    #[test]
    fn parses_verified_tls_material_only_for_non_interactive_profile_add() {
        let cli = Cli::try_parse_from([
            "reprodb",
            "profile",
            "add",
            "production",
            "--non-interactive",
            "--username",
            "readonly",
            "--password-stdin",
            "--production",
            "--tls",
            "verify-identity",
            "--tls-ca",
            "/etc/reprodb/ca.pem",
            "--tls-cert",
            "/etc/reprodb/client.pem",
            "--tls-key",
            "/etc/reprodb/client-key.pem",
        ])
        .unwrap();
        let Commands::Profile(profile) = cli.command else {
            panic!("expected profile command")
        };
        let ProfileCommands::Add(profile) = profile.command else {
            panic!("expected profile add")
        };

        assert_eq!(profile.tls, CliTlsMode::VerifyIdentity);
        assert_eq!(
            profile.tls_ca.as_deref(),
            Some(std::path::Path::new("/etc/reprodb/ca.pem"))
        );
        assert!(profile.tls_cert.is_some());
        assert!(profile.tls_key.is_some());
    }

    #[test]
    fn rejects_unpaired_or_interactive_client_tls_material() {
        assert!(
            Cli::try_parse_from([
                "reprodb",
                "profile",
                "add",
                "source",
                "--tls-cert",
                "/tmp/client.pem",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "reprodb",
                "profile",
                "add",
                "source",
                "--non-interactive",
                "--tls-cert",
                "/tmp/client.pem",
            ])
            .is_err()
        );
    }

    #[test]
    fn parses_restore_with_managed_dump_id() {
        let cli =
            Cli::try_parse_from(["reprodb", "restore", "acme", "--dump-id", "dump-123"]).unwrap();

        let Commands::Restore(arguments) = cli.command else {
            panic!("expected restore command");
        };
        assert_eq!(arguments.database, "acme");
        assert_eq!(arguments.dump_id.as_deref(), Some("dump-123"));
        assert!(arguments.target.is_none());
        assert!(arguments.target_database.is_none());
    }

    #[test]
    fn parses_a_custom_local_database_for_pull_and_restore() {
        let pull = Cli::try_parse_from([
            "reprodb",
            "pull",
            "acme",
            "--database",
            "acme_production_debug",
        ])
        .unwrap();
        let Commands::Pull(pull) = pull.command else {
            panic!("expected pull command")
        };
        assert_eq!(
            pull.target_database.as_deref(),
            Some("acme_production_debug")
        );

        let restore = Cli::try_parse_from([
            "reprodb",
            "restore",
            "acme",
            "--dump-id",
            "550e8400-e29b-41d4-a716-446655440000",
            "--database",
            "acme_production_debug",
        ])
        .unwrap();
        let Commands::Restore(restore) = restore.command else {
            panic!("expected restore command")
        };
        assert_eq!(
            restore.target_database.as_deref(),
            Some("acme_production_debug")
        );
    }

    #[test]
    fn parses_a_configured_target_for_pull_and_restore() {
        let pull =
            Cli::try_parse_from(["reprodb", "pull", "acme", "--target", "mysql-target"]).unwrap();
        let Commands::Pull(pull) = pull.command else {
            panic!("expected pull command")
        };
        assert_eq!(pull.target.as_deref(), Some("mysql-target"));

        let restore = Cli::try_parse_from([
            "reprodb",
            "restore",
            "acme",
            "--dump-id",
            "550e8400-e29b-41d4-a716-446655440000",
            "--target",
            "mysql-target",
        ])
        .unwrap();
        let Commands::Restore(restore) = restore.command else {
            panic!("expected restore command")
        };
        assert_eq!(restore.target.as_deref(), Some("mysql-target"));
    }
}
