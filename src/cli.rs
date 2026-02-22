use clap::{ArgAction, Parser, Subcommand};

/// nuance — A module manager for Nushell
#[derive(Parser, Debug)]
#[command(
    name = "nuance",
    version,
    about = "A module manager for Nushell",
    disable_version_flag = true
)]
pub struct Cli {
    #[arg(
        short = 'v',
        short_alias = 'V',
        long = "version",
        action = ArgAction::Version,
        help = "Print version"
    )]
    pub version: Option<bool>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Create a new mod.toml in the current directory
    Init {
        /// Package name (defaults to current directory name)
        #[arg(long)]
        name: Option<String>,

        /// Package version
        #[arg(long, default_value = "0.1.0")]
        version: String,

        /// Package description
        #[arg(long)]
        description: Option<String>,
    },

    /// Resolve and install dependencies from mod.toml
    Install {
        /// Install global modules (from ~/.config/nuance/config.toml)
        #[arg(short = 'g', long)]
        global: bool,

        /// Use lockfile only; error if missing or stale
        #[arg(long)]
        frozen: bool,
    },

    /// Re-resolve all dependencies (ignore existing lockfile)
    Update,

    /// Add a package from a git URL or owner/repo shorthand
    Add {
        /// Add to global config instead of local mod.toml
        #[arg(short = 'g', long)]
        global: bool,

        /// Git URL (e.g. https://github.com/user/nu-module) or owner/repo shorthand
        url: String,

        /// Pin to a specific tag
        #[arg(long)]
        tag: Option<String>,

        /// Pin to a specific commit SHA
        #[arg(long)]
        rev: Option<String>,

        /// Track a branch
        #[arg(long)]
        branch: Option<String>,
    },

    /// Remove a package from mod.toml and .nu_modules/
    #[command(visible_alias = "rm")]
    Remove {
        /// Remove from global config instead of local mod.toml
        #[arg(short = 'g', long)]
        global: bool,

        /// Package name to remove
        name: String,
    },

    /// List installed modules (project-local if mod.toml exists, otherwise global)
    #[command(visible_alias = "ls")]
    List,

    /// Print the installed nuance version
    Version,

    /// Print the Nushell env_change hook for auto-activating nuance projects
    Hook,
}

pub fn parse() -> Cli {
    Cli::parse()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    #[test]
    fn remove_alias_parses() {
        let cli = Cli::try_parse_from(["nuance", "rm", "nu-utils"]).unwrap();
        match cli.command {
            Commands::Remove { global, name } => {
                assert!(!global);
                assert_eq!(name, "nu-utils");
            }
            _ => panic!("expected remove command"),
        }
    }

    #[test]
    fn list_alias_parses() {
        let cli = Cli::try_parse_from(["nuance", "ls"]).unwrap();
        assert!(matches!(cli.command, Commands::List));
    }

    #[test]
    fn version_subcommand_parses() {
        let cli = Cli::try_parse_from(["nuance", "version"]).unwrap();
        assert!(matches!(cli.command, Commands::Version));
    }

    #[test]
    fn short_v_displays_version() {
        let err = Cli::try_parse_from(["nuance", "-v"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::DisplayVersion);
    }

    #[test]
    fn short_upper_v_displays_version() {
        let err = Cli::try_parse_from(["nuance", "-V"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::DisplayVersion);
    }
}
