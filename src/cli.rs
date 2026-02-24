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
        /// Install global modules/scripts (from ~/.config/quiver/config.toml)
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

    /// Add a script from a git URL or owner/repo shorthand
    AddScript {
        /// Add to global config instead of local nupackage.toml
        #[arg(short = 'g', long)]
        global: bool,

        /// Install global script into autoload without prompting
        #[arg(long)]
        autoload: bool,

        /// Git URL/owner-repo source, or a full blob URL
        url: String,

        /// Repository-relative path to the script file to install.
        ///
        /// Optional when `url` is a full blob URL
        path: Option<String>,

        /// Local script name (defaults to script file stem)
        #[arg(long)]
        name: Option<String>,

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

    /// Remove a module dependency from nupackage.toml and .nu_modules/
    #[command(visible_alias = "rm")]
    Remove {
        /// Remove from global config instead of local nupackage.toml
        #[arg(short = 'g', long)]
        global: bool,

        /// Package name to remove
        name: String,
    },

    /// Remove a script dependency from local nupackage.toml/.nu_scripts or global config
    RemoveScript {
        /// Remove from global config instead of local config
        #[arg(short = 'g', long)]
        global: bool,

        /// Script dependency name to remove
        name: String,
    },

    /// List installed dependencies (project-local if nupackage.toml exists, otherwise global modules/scripts)
    #[command(visible_alias = "ls")]
    List,

    /// Print the installed quiver version
    Version,

    /// Print the Nushell env_change hook for auto-activating quiver projects
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
    fn add_script_parses() {
        let cli = Cli::try_parse_from([
            "quiver",
            "add-script",
            "user/repo",
            "scripts/quickfix.nu",
            "--name",
            "quickfix",
        ])
        .unwrap();
        match cli.command {
            Commands::AddScript {
                global,
                autoload,
                url,
                path,
                name,
                ..
            } => {
                assert!(!global);
                assert!(!autoload);
                assert_eq!(url, "user/repo");
                assert_eq!(path.as_deref(), Some("scripts/quickfix.nu"));
                assert_eq!(name.as_deref(), Some("quickfix"));
            }
            _ => panic!("expected add-script command"),
        }
    }

    #[test]
    fn add_script_global_parses() {
        let cli = Cli::try_parse_from([
            "quiver",
            "add-script",
            "--global",
            "--autoload",
            "user/repo",
            "scripts/quickfix.nu",
        ])
        .unwrap();
        match cli.command {
            Commands::AddScript {
                global, autoload, ..
            } => {
                assert!(global);
                assert!(autoload);
            }
            _ => panic!("expected add-script command"),
        }
    }

    #[test]
    fn add_script_blob_url_without_explicit_path_parses() {
        let cli = Cli::try_parse_from([
            "quiver",
            "add-script",
            "https://github.com/nushell/nu_scripts/blob/main/sourced/webscraping/twitter.nu",
        ])
        .unwrap();
        match cli.command {
            Commands::AddScript { url, path, .. } => {
                assert_eq!(
                    url,
                    "https://github.com/nushell/nu_scripts/blob/main/sourced/webscraping/twitter.nu"
                );
                assert!(path.is_none());
            }
            _ => panic!("expected add-script command"),
        }
    }

    #[test]
    fn remove_script_parses() {
        let cli = Cli::try_parse_from(["quiver", "remove-script", "quickfix"]).unwrap();
        match cli.command {
            Commands::RemoveScript { global, name } => {
                assert!(!global);
                assert_eq!(name, "quickfix");
            }
            _ => panic!("expected remove-script command"),
        }
    }

    #[test]
    fn remove_script_global_parses() {
        let cli = Cli::try_parse_from(["quiver", "remove-script", "-g", "quickfix"]).unwrap();
        match cli.command {
            Commands::RemoveScript { global, name } => {
                assert!(global);
                assert_eq!(name, "quickfix");
            }
            _ => panic!("expected remove-script command"),
        }
    }

    #[test]
    fn short_v_displays_version() {
        let err = Cli::try_parse_from(["quiver", "-v"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::DisplayVersion);
    }

    #[test]
    fn short_upper_v_displays_version() {
        let err = Cli::try_parse_from(["quiver", "-V"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::DisplayVersion);
    }
}
