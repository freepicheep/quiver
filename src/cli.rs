use clap::{ArgAction, Parser, Subcommand};
use std::ffi::OsString;
use std::path::Path;

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

#[derive(Parser, Debug)]
#[command(
    name = "qvx",
    version,
    about = "Run a command exported by a remote Nushell module",
    disable_version_flag = true
)]
struct QvxCli {
    #[arg(
        short = 'v',
        short_alias = 'V',
        long = "version",
        action = ArgAction::Version,
        help = "Print version"
    )]
    version: Option<bool>,

    /// Pin to a specific tag
    #[arg(long)]
    tag: Option<String>,

    /// Pin to a specific commit SHA
    #[arg(long)]
    rev: Option<String>,

    /// Track a branch
    #[arg(long)]
    branch: Option<String>,

    /// Git URL or owner/repo shorthand, optionally suffixed with @tag
    source: String,

    /// Exported Nushell command to invoke
    command: String,

    /// Arguments to pass to the exported command
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
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

        /// Nushell version requirement for this project (e.g. 0.109.0, >=0.109,<0.111)
        #[arg(long = "nu-version")]
        nu_version: Option<String>,

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

        /// Allow unsigned release assets (insecure; disabled in --frozen mode)
        #[arg(long)]
        allow_unsigned: bool,

        /// Disable cargo source-build fallback for plugins
        #[arg(long)]
        no_build_fallback: bool,
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
        /// Add to global config instead of local nupackage.toml
        #[arg(short = 'g', long)]
        global: bool,

        /// Git URL, owner/repo shorthand, or core plugin name (e.g. polars)
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

    /// Run a command exported by a remote Nushell module
    Qvx {
        /// Pin to a specific tag
        #[arg(long)]
        tag: Option<String>,

        /// Pin to a specific commit SHA
        #[arg(long)]
        rev: Option<String>,

        /// Track a branch
        #[arg(long)]
        branch: Option<String>,

        /// Git URL or owner/repo shorthand, optionally suffixed with @tag
        source: String,

        /// Exported Nushell command to invoke
        command: String,

        /// Arguments to pass to the exported command
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

pub fn parse() -> Cli {
    let args: Vec<OsString> = std::env::args_os().collect();
    if invoked_as_qvx(args.first()) {
        let qvx = QvxCli::parse_from(args);
        return Cli {
            version: qvx.version,
            command: Commands::Qvx {
                tag: qvx.tag,
                rev: qvx.rev,
                branch: qvx.branch,
                source: qvx.source,
                command: qvx.command,
                args: qvx.args,
            },
        };
    }
    Cli::parse_from(args)
}

fn invoked_as_qvx(arg0: Option<&OsString>) -> bool {
    arg0.and_then(|arg| Path::new(arg).file_stem())
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.eq_ignore_ascii_case("qvx"))
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
    fn qvx_subcommand_parses_source_command_and_args() {
        let cli = Cli::try_parse_from([
            "quiver",
            "qvx",
            "freepicheep/nu-doc-gen@v1.2.0",
            "generate-doc-site",
            "nu-salesforce",
            ".",
        ])
        .unwrap();
        match cli.command {
            Commands::Qvx {
                tag,
                rev,
                branch,
                source,
                command,
                args,
            } => {
                assert_eq!(tag, None);
                assert_eq!(rev, None);
                assert_eq!(branch, None);
                assert_eq!(source, "freepicheep/nu-doc-gen@v1.2.0");
                assert_eq!(command, "generate-doc-site");
                assert_eq!(args, vec!["nu-salesforce", "."]);
            }
            _ => panic!("expected qvx command"),
        }
    }

    #[test]
    fn qvx_subcommand_preserves_hyphenated_args() {
        let cli = Cli::try_parse_from([
            "quiver",
            "qvx",
            "--branch",
            "main",
            "freepicheep/nu-doc-gen",
            "generate-doc-site",
            "--theme",
            "plain",
        ])
        .unwrap();
        match cli.command {
            Commands::Qvx {
                branch,
                source,
                command,
                args,
                ..
            } => {
                assert_eq!(branch, Some("main".to_string()));
                assert_eq!(source, "freepicheep/nu-doc-gen");
                assert_eq!(command, "generate-doc-site");
                assert_eq!(args, vec!["--theme", "plain"]);
            }
            _ => panic!("expected qvx command"),
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
            "--global",
            "nushell/nu_plugin_inc",
            "--tag",
            "v0.91.0",
            "--bin",
            "nu_plugin_inc",
        ])
        .unwrap();
        match cli.command {
            Commands::AddPlugin {
                global,
                url,
                tag,
                rev,
                branch,
                bin,
            } => {
                assert!(global);
                assert_eq!(url, "nushell/nu_plugin_inc");
                assert_eq!(tag.as_deref(), Some("v0.91.0"));
                assert!(rev.is_none());
                assert!(branch.is_none());
                assert_eq!(bin.as_deref(), Some("nu_plugin_inc"));
            }
            _ => panic!("expected add-plugin command"),
        }
    }

    #[test]
    fn init_parses_with_nu_version() {
        let cli = Cli::try_parse_from(["quiver", "init", "--nu-version", "0.109.0"]).unwrap();
        match cli.command {
            Commands::Init {
                name,
                version,
                nu_version,
                description,
            } => {
                assert!(name.is_none());
                assert_eq!(version, "0.1.0");
                assert_eq!(nu_version.as_deref(), Some("0.109.0"));
                assert!(description.is_none());
            }
            _ => panic!("expected init command"),
        }
    }

    #[test]
    fn install_parses_security_flags() {
        let cli = Cli::try_parse_from([
            "quiver",
            "install",
            "--allow-unsigned",
            "--no-build-fallback",
        ])
        .unwrap();
        match cli.command {
            Commands::Install {
                global,
                frozen,
                allow_unsigned,
                no_build_fallback,
            } => {
                assert!(!global);
                assert!(!frozen);
                assert!(allow_unsigned);
                assert!(no_build_fallback);
            }
            _ => panic!("expected install command"),
        }
    }
}
