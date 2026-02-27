use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{QuiverError, Result};
use crate::manifest::DependencySpec;

const DEFAULT_GIT_PROVIDER: &str = "github";

fn default_git_provider() -> String {
    DEFAULT_GIT_PROVIDER.to_string()
}

fn default_install_mode() -> InstallMode {
    if cfg!(windows) {
        InstallMode::Hardlink
    } else {
        InstallMode::Clone
    }
}

fn known_provider_base_url(provider: &str) -> Option<&'static str> {
    match provider {
        "github" => Some("https://github.com"),
        "gitlab" => Some("https://gitlab.com"),
        "codeberg" => Some("https://codeberg.org"),
        "bitbucket" => Some("https://bitbucket.org"),
        _ => None,
    }
}

fn normalize_provider_base_url(provider: &str) -> Option<String> {
    let trimmed = provider.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }

    let lowercase = trimmed.to_ascii_lowercase();
    if let Some(base) = known_provider_base_url(&lowercase) {
        return Some(base.to_string());
    }

    if trimmed.starts_with("https://") || trimmed.starts_with("http://") {
        return Some(trimmed.to_string());
    }

    if trimmed.contains('.') && !trimmed.contains('/') && !trimmed.contains(' ') {
        return Some(format!("https://{trimmed}"));
    }

    None
}

/// The global quiver config file: `~/.config/quiver/config.toml`.
///
/// Tracks globally-installed modules and optional path overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstallMode {
    Clone,
    Hardlink,
    Copy,
}

/// The global quiver config file: `~/.config/quiver/config.toml`.
///
/// Tracks globally-installed modules and optional path overrides.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modules_dir: Option<String>,

    #[serde(default = "default_git_provider")]
    pub default_git_provider: String,

    #[serde(default = "default_install_mode")]
    pub install_mode: InstallMode,

    #[serde(default)]
    pub dependencies: HashMap<String, DependencySpec>,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            modules_dir: None,
            default_git_provider: default_git_provider(),
            install_mode: default_install_mode(),
            dependencies: HashMap::new(),
        }
    }
}

impl GlobalConfig {
    /// Load the global config, creating it with defaults if it doesn't exist.
    pub fn load() -> Result<Self> {
        let path = global_config_path()?;

        if !path.exists() {
            let config = GlobalConfig::default();
            config.save()?;
            return Ok(config);
        }

        Self::load_from_path(&path)
    }

    /// Load global config if present, otherwise return defaults without writing.
    pub fn load_or_default() -> Result<Self> {
        let path = global_config_path()?;

        if !path.exists() {
            return Ok(GlobalConfig::default());
        }

        Self::load_from_path(&path)
    }

    fn load_from_path(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(&path)?;
        let config: GlobalConfig = toml::from_str(&content)
            .map_err(|e| QuiverError::Config(format!("failed to parse {}: {e}", path.display())))?;
        Ok(config)
    }

    /// Save the global config back to disk.
    pub fn save(&self) -> Result<()> {
        let path = global_config_path()?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let content = toml::to_string_pretty(self)
            .map_err(|e| QuiverError::Config(format!("failed to serialize config: {e}")))?;
        std::fs::write(&path, content)?;
        Ok(())
    }

    /// Returns the directory where global modules should be installed.
    ///
    /// Uses the `modules_dir` override if set, otherwise falls back to
    /// `~/.config/nushell/vendor/quiver/modules/`.
    pub fn modules_dir(&self) -> Result<PathBuf> {
        if let Some(ref custom) = self.modules_dir {
            Ok(PathBuf::from(custom))
        } else {
            global_modules_dir()
        }
    }

    /// Resolve the configured default git provider to a base URL.
    pub fn default_git_provider_base_url(&self) -> Result<String> {
        normalize_provider_base_url(&self.default_git_provider).ok_or_else(|| {
            QuiverError::Config(format!(
                "unsupported default_git_provider '{}'; use one of github, gitlab, codeberg, bitbucket, or a custom host like git.example.com",
                self.default_git_provider
            ))
        })
    }
}

/// Returns the global config directory: `~/.config/quiver/`.
pub fn global_config_dir() -> Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| QuiverError::Config("could not determine home directory".to_string()))?;
    Ok(home.join(".config").join("quiver"))
}

/// Returns the path to the global config file: `~/.config/quiver/config.toml`.
pub fn global_config_path() -> Result<PathBuf> {
    Ok(global_config_dir()?.join("config.toml"))
}

/// Returns the path to the global lockfile: `~/.config/quiver/config.lock`.
pub fn global_lock_path() -> Result<PathBuf> {
    Ok(global_config_dir()?.join("config.lock"))
}

/// Returns the root install directory.
///
/// Uses `~/.local/share/quiver/installs/` on macOS/Linux and
/// `%APPDATA%/quiver/installs/` on Windows.
pub fn installs_root_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let data = dirs::data_dir()
            .ok_or_else(|| QuiverError::Config("could not determine data directory".to_string()))?;
        return Ok(data.join("quiver").join("installs"));
    }

    #[cfg(not(windows))]
    {
        let home = dirs::home_dir()
            .ok_or_else(|| QuiverError::Config("could not determine home directory".to_string()))?;
        return Ok(home.join(".local").join("share").join("quiver").join("installs"));
    }
}

/// Returns the shared module install store:
/// `~/.local/share/quiver/installs/modules/` on Linux.
pub fn installs_modules_dir() -> Result<PathBuf> {
    Ok(installs_root_dir()?.join("modules"))
}

/// Returns the shared plugin install store:
/// `~/.local/share/quiver/installs/plugins/` on Linux.
pub fn installs_plugins_dir() -> Result<PathBuf> {
    Ok(installs_root_dir()?.join("plugins"))
}

/// Returns the shared Nushell-version install store:
/// `~/.local/share/quiver/installs/nu_versions/` on macOS/Linux.
pub fn installs_nu_versions_dir() -> Result<PathBuf> {
    Ok(installs_root_dir()?.join("nu_versions"))
}

/// Returns the default global modules directory, using the platform config
/// directory (where Nushell stores its config) + `vendor/quiver/modules/`.
///
/// e.g. `~/Library/Application Support/nushell/vendor/quiver/modules/` on macOS,
///      `~/.config/nushell/vendor/quiver/modules/` on Linux.
pub fn global_modules_dir() -> Result<PathBuf> {
    let config = dirs::config_dir()
        .ok_or_else(|| QuiverError::Config("could not determine config directory".to_string()))?;
    Ok(config
        .join("nushell")
        .join("vendor")
        .join("quiver")
        .join("modules"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let config = GlobalConfig {
            modules_dir: None,
            default_git_provider: "github".to_string(),
            install_mode: default_install_mode(),
            dependencies: HashMap::from([(
                "nu-utils".to_string(),
                DependencySpec {
                    git: "https://github.com/user/nu-utils".to_string(),
                    tag: Some("v1.0.0".to_string()),
                    rev: None,
                    branch: None,
                },
            )]),
        };

        let serialized = toml::to_string_pretty(&config).unwrap();
        let parsed: GlobalConfig = toml::from_str(&serialized).unwrap();

        assert_eq!(parsed.dependencies.len(), 1);
        assert!(parsed.dependencies.contains_key("nu-utils"));
        assert!(parsed.modules_dir.is_none());
        assert_eq!(parsed.default_git_provider, "github");
        assert_eq!(parsed.install_mode, default_install_mode());
    }

    #[test]
    fn round_trip_with_override() {
        let config = GlobalConfig {
            modules_dir: Some("/custom/path".to_string()),
            default_git_provider: "gitlab".to_string(),
            install_mode: InstallMode::Copy,
            dependencies: HashMap::new(),
        };

        let serialized = toml::to_string_pretty(&config).unwrap();
        let parsed: GlobalConfig = toml::from_str(&serialized).unwrap();

        assert_eq!(parsed.modules_dir.as_deref(), Some("/custom/path"));
        assert_eq!(parsed.default_git_provider, "gitlab");
        assert_eq!(parsed.install_mode, InstallMode::Copy);
    }

    #[test]
    fn modules_dir_custom() {
        let config = GlobalConfig {
            modules_dir: Some("/custom/modules".to_string()),
            default_git_provider: "github".to_string(),
            install_mode: default_install_mode(),
            dependencies: HashMap::new(),
        };
        assert_eq!(
            config.modules_dir().unwrap(),
            PathBuf::from("/custom/modules")
        );
    }

    #[test]
    fn modules_dir_default() {
        let config = GlobalConfig {
            modules_dir: None,
            default_git_provider: "github".to_string(),
            install_mode: default_install_mode(),
            dependencies: HashMap::new(),
        };
        let dir = config.modules_dir().unwrap();
        // Should end with nushell/vendor/quiver/modules
        assert!(dir.ends_with("nushell/vendor/quiver/modules"));
    }

    #[test]
    fn config_dir_paths() {
        // These should not error on any platform with a home directory
        let dir = global_config_dir().unwrap();
        assert!(dir.ends_with("quiver"));

        let path = global_config_path().unwrap();
        assert!(path.ends_with("quiver/config.toml"));

        let lock = global_lock_path().unwrap();
        assert!(lock.ends_with("quiver/config.lock"));
    }

    #[test]
    fn install_store_paths() {
        let root = installs_root_dir().unwrap();
        assert!(root.ends_with("quiver/installs"));

        let modules = installs_modules_dir().unwrap();
        assert!(modules.ends_with("quiver/installs/modules"));

        let plugins = installs_plugins_dir().unwrap();
        assert!(plugins.ends_with("quiver/installs/plugins"));

        let nu_versions = installs_nu_versions_dir().unwrap();
        assert!(nu_versions.ends_with("quiver/installs/nu_versions"));
    }

    #[test]
    fn missing_provider_defaults_to_github() {
        let toml = r#"
modules_dir = "/tmp/quiver-modules"

[dependencies]
"#;
        let parsed: GlobalConfig = toml::from_str(toml).unwrap();
        assert_eq!(parsed.default_git_provider, "github");
        assert_eq!(parsed.install_mode, default_install_mode());
    }

    #[test]
    fn default_provider_base_url_resolves_known_aliases() {
        let mut config = GlobalConfig::default();
        assert_eq!(
            config.default_git_provider_base_url().unwrap(),
            "https://github.com"
        );

        config.default_git_provider = "gitlab".to_string();
        assert_eq!(
            config.default_git_provider_base_url().unwrap(),
            "https://gitlab.com"
        );
    }

    #[test]
    fn default_provider_base_url_supports_custom_domain() {
        let config = GlobalConfig {
            modules_dir: None,
            default_git_provider: "git.example.com".to_string(),
            install_mode: default_install_mode(),
            dependencies: HashMap::new(),
        };
        assert_eq!(
            config.default_git_provider_base_url().unwrap(),
            "https://git.example.com"
        );
    }

    #[test]
    fn default_provider_base_url_rejects_unknown_provider() {
        let config = GlobalConfig {
            modules_dir: None,
            default_git_provider: "not-a-provider".to_string(),
            install_mode: default_install_mode(),
            dependencies: HashMap::new(),
        };
        let err = config.default_git_provider_base_url().unwrap_err();
        assert!(err.to_string().contains("unsupported default_git_provider"));
    }

    #[test]
    fn install_mode_round_trip() {
        let config = GlobalConfig {
            modules_dir: None,
            default_git_provider: "github".to_string(),
            install_mode: InstallMode::Hardlink,
            dependencies: HashMap::new(),
        };

        let serialized = toml::to_string_pretty(&config).unwrap();
        let parsed: GlobalConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(parsed.install_mode, InstallMode::Hardlink);
    }
}
