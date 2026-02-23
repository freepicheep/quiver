use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{NuanceError, Result};
use crate::manifest::{DependencySpec, ScriptDependencySpec};

const DEFAULT_GIT_PROVIDER: &str = "github";

fn default_git_provider() -> String {
    DEFAULT_GIT_PROVIDER.to_string()
}

fn is_false(value: &bool) -> bool {
    !*value
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

/// The global nuance config file: `~/.config/nuance/config.toml`.
///
/// Tracks globally-installed modules/scripts and optional path overrides.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modules_dir: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scripts_dir: Option<String>,

    #[serde(default = "default_git_provider")]
    pub default_git_provider: String,

    #[serde(default)]
    pub dependencies: HashMap<String, DependencySpec>,

    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub scripts: HashMap<String, GlobalScriptDependencySpec>,
}

/// A single script specification from `[scripts]` in global config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalScriptDependencySpec {
    pub git: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub autoload: bool,
}

impl GlobalScriptDependencySpec {
    pub fn to_script_dependency_spec(&self) -> ScriptDependencySpec {
        ScriptDependencySpec {
            git: self.git.clone(),
            path: self.path.clone(),
            tag: self.tag.clone(),
            rev: self.rev.clone(),
            branch: self.branch.clone(),
        }
    }

    pub fn from_script_dependency_spec(spec: ScriptDependencySpec, autoload: bool) -> Self {
        Self {
            git: spec.git,
            path: spec.path,
            tag: spec.tag,
            rev: spec.rev,
            branch: spec.branch,
            autoload,
        }
    }
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            modules_dir: None,
            scripts_dir: None,
            default_git_provider: default_git_provider(),
            dependencies: HashMap::new(),
            scripts: HashMap::new(),
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
            .map_err(|e| NuanceError::Config(format!("failed to parse {}: {e}", path.display())))?;
        Ok(config)
    }

    /// Save the global config back to disk.
    pub fn save(&self) -> Result<()> {
        let path = global_config_path()?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let content = toml::to_string_pretty(self)
            .map_err(|e| NuanceError::Config(format!("failed to serialize config: {e}")))?;
        std::fs::write(&path, content)?;
        Ok(())
    }

    /// Returns the directory where global modules should be installed.
    ///
    /// Uses the `modules_dir` override if set, otherwise falls back to
    /// `~/.config/nushell/vendor/nuance/modules/`.
    pub fn modules_dir(&self) -> Result<PathBuf> {
        if let Some(ref custom) = self.modules_dir {
            Ok(PathBuf::from(custom))
        } else {
            global_modules_dir()
        }
    }

    /// Returns the directory where global scripts should be installed.
    ///
    /// Uses the `scripts_dir` override if set, otherwise falls back to
    /// `~/.config/nushell/vendor/nuance/scripts/`.
    pub fn scripts_dir(&self) -> Result<PathBuf> {
        if let Some(ref custom) = self.scripts_dir {
            Ok(PathBuf::from(custom))
        } else {
            global_scripts_dir()
        }
    }

    /// Returns the directory where global autoload scripts should be installed.
    ///
    /// Uses `scripts_dir/autoload` when `scripts_dir` is overridden.
    pub fn scripts_autoload_dir(&self) -> Result<PathBuf> {
        if let Some(ref custom) = self.scripts_dir {
            Ok(PathBuf::from(custom).join("autoload"))
        } else {
            global_scripts_autoload_dir()
        }
    }

    /// Returns whether the named global script should install into autoload.
    pub fn script_is_autoload(&self, name: &str) -> bool {
        self.scripts
            .get(name)
            .map(|script| script.autoload)
            .unwrap_or(false)
    }

    /// Resolve the configured default git provider to a base URL.
    pub fn default_git_provider_base_url(&self) -> Result<String> {
        normalize_provider_base_url(&self.default_git_provider).ok_or_else(|| {
            NuanceError::Config(format!(
                "unsupported default_git_provider '{}'; use one of github, gitlab, codeberg, bitbucket, or a custom host like git.example.com",
                self.default_git_provider
            ))
        })
    }
}

/// Returns the global config directory: `~/.config/nuance/`.
pub fn global_config_dir() -> Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| NuanceError::Config("could not determine home directory".to_string()))?;
    Ok(home.join(".config").join("nuance"))
}

/// Returns the path to the global config file: `~/.config/nuance/config.toml`.
pub fn global_config_path() -> Result<PathBuf> {
    Ok(global_config_dir()?.join("config.toml"))
}

/// Returns the path to the global lockfile: `~/.config/nuance/config.lock`.
pub fn global_lock_path() -> Result<PathBuf> {
    Ok(global_config_dir()?.join("config.lock"))
}

/// Returns the default global modules directory, using the platform config
/// directory (where Nushell stores its config) + `vendor/nuance/modules/`.
///
/// e.g. `~/Library/Application Support/nushell/vendor/nuance/modules/` on macOS,
///      `~/.config/nushell/vendor/nuance/modules/` on Linux.
pub fn global_modules_dir() -> Result<PathBuf> {
    let config = dirs::config_dir()
        .ok_or_else(|| NuanceError::Config("could not determine config directory".to_string()))?;
    Ok(config
        .join("nushell")
        .join("vendor")
        .join("nuance")
        .join("modules"))
}

/// Returns the default global scripts directory, using the platform config
/// directory + `nushell/vendor/nuance/scripts/`.
///
/// e.g. `~/Library/Application Support/nushell/vendor/nuance/scripts/` on macOS,
///      `~/.config/nushell/vendor/nuance/scripts/` on Linux.
pub fn global_scripts_dir() -> Result<PathBuf> {
    let config = dirs::config_dir()
        .ok_or_else(|| NuanceError::Config("could not determine config directory".to_string()))?;
    Ok(config
        .join("nushell")
        .join("vendor")
        .join("nuance")
        .join("scripts"))
}

/// Returns the default global autoload scripts directory.
///
/// e.g. `~/Library/Application Support/nushell/vendor/nuance/scripts/autoload/` on macOS,
///      `~/.config/nushell/vendor/nuance/scripts/autoload/` on Linux.
pub fn global_scripts_autoload_dir() -> Result<PathBuf> {
    Ok(global_scripts_dir()?.join("autoload"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let config = GlobalConfig {
            modules_dir: None,
            scripts_dir: None,
            default_git_provider: "github".to_string(),
            dependencies: HashMap::from([(
                "nu-utils".to_string(),
                DependencySpec {
                    git: "https://github.com/user/nu-utils".to_string(),
                    tag: Some("v1.0.0".to_string()),
                    rev: None,
                    branch: None,
                },
            )]),
            scripts: HashMap::from([(
                "quickfix".to_string(),
                GlobalScriptDependencySpec {
                    git: "https://github.com/user/nu-scripts".to_string(),
                    path: "scripts/quickfix.nu".to_string(),
                    tag: Some("v0.2.0".to_string()),
                    rev: None,
                    branch: None,
                    autoload: true,
                },
            )]),
        };

        let serialized = toml::to_string_pretty(&config).unwrap();
        let parsed: GlobalConfig = toml::from_str(&serialized).unwrap();

        assert_eq!(parsed.dependencies.len(), 1);
        assert!(parsed.dependencies.contains_key("nu-utils"));
        assert_eq!(parsed.scripts.len(), 1);
        assert!(parsed.scripts.contains_key("quickfix"));
        assert!(parsed.scripts.get("quickfix").unwrap().autoload);
        assert!(parsed.modules_dir.is_none());
        assert!(parsed.scripts_dir.is_none());
        assert_eq!(parsed.default_git_provider, "github");
    }

    #[test]
    fn round_trip_with_override() {
        let config = GlobalConfig {
            modules_dir: Some("/custom/path".to_string()),
            scripts_dir: Some("/custom/scripts".to_string()),
            default_git_provider: "gitlab".to_string(),
            dependencies: HashMap::new(),
            scripts: HashMap::new(),
        };

        let serialized = toml::to_string_pretty(&config).unwrap();
        let parsed: GlobalConfig = toml::from_str(&serialized).unwrap();

        assert_eq!(parsed.modules_dir.as_deref(), Some("/custom/path"));
        assert_eq!(parsed.scripts_dir.as_deref(), Some("/custom/scripts"));
        assert_eq!(parsed.default_git_provider, "gitlab");
    }

    #[test]
    fn modules_dir_custom() {
        let config = GlobalConfig {
            modules_dir: Some("/custom/modules".to_string()),
            scripts_dir: None,
            default_git_provider: "github".to_string(),
            dependencies: HashMap::new(),
            scripts: HashMap::new(),
        };
        assert_eq!(
            config.modules_dir().unwrap(),
            PathBuf::from("/custom/modules")
        );
    }

    #[test]
    fn scripts_dir_custom() {
        let config = GlobalConfig {
            modules_dir: None,
            scripts_dir: Some("/custom/scripts".to_string()),
            default_git_provider: "github".to_string(),
            dependencies: HashMap::new(),
            scripts: HashMap::new(),
        };
        assert_eq!(
            config.scripts_dir().unwrap(),
            PathBuf::from("/custom/scripts")
        );
    }

    #[test]
    fn modules_dir_default() {
        let config = GlobalConfig {
            modules_dir: None,
            scripts_dir: None,
            default_git_provider: "github".to_string(),
            dependencies: HashMap::new(),
            scripts: HashMap::new(),
        };
        let dir = config.modules_dir().unwrap();
        // Should end with nushell/vendor/nuance/modules
        assert!(dir.ends_with("nushell/vendor/nuance/modules"));
    }

    #[test]
    fn scripts_dir_default() {
        let config = GlobalConfig {
            modules_dir: None,
            scripts_dir: None,
            default_git_provider: "github".to_string(),
            dependencies: HashMap::new(),
            scripts: HashMap::new(),
        };
        let dir = config.scripts_dir().unwrap();
        assert!(dir.ends_with("nushell/vendor/nuance/scripts"));
    }

    #[test]
    fn scripts_autoload_dir_default() {
        let config = GlobalConfig {
            modules_dir: None,
            scripts_dir: None,
            default_git_provider: "github".to_string(),
            dependencies: HashMap::new(),
            scripts: HashMap::new(),
        };
        let dir = config.scripts_autoload_dir().unwrap();
        assert!(dir.ends_with("nushell/vendor/nuance/scripts/autoload"));
    }

    #[test]
    fn scripts_autoload_dir_uses_override() {
        let config = GlobalConfig {
            modules_dir: None,
            scripts_dir: Some("/custom/scripts".to_string()),
            default_git_provider: "github".to_string(),
            dependencies: HashMap::new(),
            scripts: HashMap::new(),
        };
        assert_eq!(
            config.scripts_autoload_dir().unwrap(),
            PathBuf::from("/custom/scripts/autoload")
        );
    }

    #[test]
    fn config_dir_paths() {
        // These should not error on any platform with a home directory
        let dir = global_config_dir().unwrap();
        assert!(dir.ends_with("nuance"));

        let path = global_config_path().unwrap();
        assert!(path.ends_with("nuance/config.toml"));

        let lock = global_lock_path().unwrap();
        assert!(lock.ends_with("nuance/config.lock"));
    }

    #[test]
    fn missing_provider_defaults_to_github() {
        let toml = r#"
modules_dir = "/tmp/nuance-modules"

[dependencies]
"#;
        let parsed: GlobalConfig = toml::from_str(toml).unwrap();
        assert_eq!(parsed.default_git_provider, "github");
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
            scripts_dir: None,
            default_git_provider: "git.example.com".to_string(),
            dependencies: HashMap::new(),
            scripts: HashMap::new(),
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
            scripts_dir: None,
            default_git_provider: "not-a-provider".to_string(),
            dependencies: HashMap::new(),
            scripts: HashMap::new(),
        };
        let err = config.default_git_provider_base_url().unwrap_err();
        assert!(err.to_string().contains("unsupported default_git_provider"));
    }

    #[test]
    fn missing_scripts_defaults_to_empty() {
        let toml = r#"
modules_dir = "/tmp/nuance-modules"

[dependencies]
"#;
        let parsed: GlobalConfig = toml::from_str(toml).unwrap();
        assert!(parsed.scripts.is_empty());
        assert!(parsed.scripts_dir.is_none());
    }
}
