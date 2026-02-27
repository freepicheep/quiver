use clap::{ArgAction, Parser, Subcommand};

/// Quiver — A module manager for Nushell
#[derive(Parser, Debug)]
#[command(
    name = "quiver",
    bin_name = "qv",
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
    /// Create a new nupackage.toml in the current directory
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

    /// Resolve and install dependencies from nupackage.toml
    Install {
        /// Install global modules (from ~/.config/quiver/config.toml)
        #[arg(short = 'g', long)]
        global: bool,

        /// Use lockfile only; error if missing or stale
        #[arg(long)]
        frozen: bool,
    },

    /// Re-resolve all dependencies (ignore existing lockfile)
    Update,

    /// Add a module dependency from a git URL or owner/repo shorthand
    Add {
        /// Add to global config instead of local nupackage.toml
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

    /// Add a plugin dependency from a git URL or owner/repo shorthand
    AddPlugin {
        /// Git URL (e.g. https://github.com/user/nu_plugin_inc) or owner/repo shorthand
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

        /// Binary target name if it differs from the repo/package name
        #[arg(long)]
        bin: Option<String>,
    },

    /// Remove a module dependency from nupackage.toml and .nu_modules/
    #[command(visible_alias = "rm")]
    Remove {
        /// Remove from global config instead of local nupackage.toml
        #[arg(short = 'g', long)]
        global: bool,

        /// Package name to remove
        name: String,
    },

    /// List installed dependencies (project-local if nupackage.toml exists, otherwise global modules)
    #[command(visible_alias = "ls")]
    List,

    /// Print the installed quiver version
    Version,

    /// Print the Nushell env_change hook for auto-activating quiver projects
    Hook,

    /// Generate editor-specific LSP configuration for Nushell
    Lsp {
        /// Editors to configure (helix, zed). If omitted, shows an interactive picker.
        #[arg(long)]
        editor: Vec<String>,
    },

    /// Run a command in the project with quiver's Nushell environment
    Run {
        /// Command and arguments to execute
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
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
        let cli = Cli::try_parse_from(["quiver", "rm", "nu-utils"]).unwrap();
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
        let cli = Cli::try_parse_from(["quiver", "ls"]).unwrap();
        assert!(matches!(cli.command, Commands::List));
    }

    #[test]
    fn version_subcommand_parses() {
        let cli = Cli::try_parse_from(["quiver", "version"]).unwrap();
        assert!(matches!(cli.command, Commands::Version));
    }

    #[test]
    fn run_subcommand_parses_with_multiple_args() {
        let cli = Cli::try_parse_from(["quiver", "run", "nu", "script.nu", "--flag"]).unwrap();
        match cli.command {
            Commands::Run { command } => {
                assert_eq!(command, vec!["nu", "script.nu", "--flag"]);
            }
            _ => panic!("expected run command"),
        }
    }

    #[test]
    fn short_v_displays_version() {
        let err = Cli::try_parse_from(["quiver", "-v"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::DisplayVersion);
    }

    #[test]
    fn short_uppercase_v_displays_version() {
        let err = Cli::try_parse_from(["quiver", "-V"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::DisplayVersion);
    }

    #[test]
    fn add_plugin_parses_with_bin() {
        let cli = Cli::try_parse_from([
            "quiver",
            "add-plugin",
            "nushell/nu_plugin_inc",
            "--tag",
            "v0.91.0",
            "--bin",
            "nu_plugin_inc",
        ])
        .unwrap();
        match cli.command {
            Commands::AddPlugin {
                url,
                tag,
                rev,
                branch,
                bin,
            } => {
                assert_eq!(url, "nushell/nu_plugin_inc");
                assert_eq!(tag.as_deref(), Some("v0.91.0"));
                assert!(rev.is_none());
                assert!(branch.is_none());
                assert_eq!(bin.as_deref(), Some("nu_plugin_inc"));
            }
            _ => panic!("expected add-plugin command"),
        }
    }
}
