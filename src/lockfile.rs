use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::error::Result;

/// The `mod.lock` lockfile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Lockfile {
    pub version: u32,
    #[serde(rename = "package")]
    pub packages: Vec<LockedPackage>,
}

/// A single locked package entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LockedPackage {
    pub name: String,
    #[serde(default, skip_serializing_if = "LockedPackageKind::is_module")]
    pub kind: LockedPackageKind,
    pub git: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    pub rev: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub sha256: String,
}

/// The kind of installed artifact in the lockfile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Default)]
#[serde(rename_all = "lowercase")]
pub enum LockedPackageKind {
    #[default]
    Module,
    Script,
}

impl LockedPackageKind {
    fn is_module(kind: &LockedPackageKind) -> bool {
        matches!(kind, LockedPackageKind::Module)
    }
}

impl Lockfile {
    /// Read a lockfile from disk.
    pub fn from_path(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Self::from_str(&content)
    }

    /// Parse a lockfile from a TOML string.
    pub fn from_str(s: &str) -> Result<Self> {
        Ok(toml::from_str(s)?)
    }

    /// Serialize the lockfile to a TOML string with the header comment.
    pub fn to_toml_string(&self) -> Result<String> {
        let body = toml::to_string_pretty(self)?;
        Ok(format!(
            "# This file is generated automatically. Do not edit.\n{body}"
        ))
    }

    /// Write the lockfile to disk.
    pub fn write_to(&self, path: &Path) -> Result<()> {
        let content = self.to_toml_string()?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// Look up a locked package by name and kind.
    pub fn find_package(&self, name: &str, kind: LockedPackageKind) -> Option<&LockedPackage> {
        self.packages
            .iter()
            .find(|p| p.name == name && p.kind == kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_lockfile() -> Lockfile {
        Lockfile {
            version: 1,
            packages: vec![
                LockedPackage {
                    name: "nu-git-utils".to_string(),
                    kind: LockedPackageKind::Module,
                    git: "https://github.com/someuser/nu-git-utils".to_string(),
                    tag: Some("v0.2.0".to_string()),
                    rev: "d4e8f1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8".to_string(),
                    path: None,
                    sha256: "abc123".to_string(),
                },
                LockedPackage {
                    name: "nu-str-extras".to_string(),
                    kind: LockedPackageKind::Module,
                    git: "https://github.com/someuser/nu-str-extras".to_string(),
                    tag: Some("v1.0.0".to_string()),
                    rev: "1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b".to_string(),
                    path: None,
                    sha256: "def456".to_string(),
                },
                LockedPackage {
                    name: "quickfix".to_string(),
                    kind: LockedPackageKind::Script,
                    git: "https://github.com/someuser/nu-scripts".to_string(),
                    tag: Some("v0.5.0".to_string()),
                    rev: "9a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b".to_string(),
                    path: Some("scripts/quickfix.nu".to_string()),
                    sha256: "ghi789".to_string(),
                },
            ],
        }
    }

    #[test]
    fn round_trip() {
        let lock = sample_lockfile();
        let serialized = lock.to_toml_string().unwrap();

        // The header comment is not part of the TOML data, strip it for parsing
        let parsed = Lockfile::from_str(&serialized).unwrap();
        assert_eq!(lock, parsed);
    }

    #[test]
    fn find_package_by_name_and_kind() {
        let lock = sample_lockfile();
        let pkg = lock
            .find_package("nu-git-utils", LockedPackageKind::Module)
            .unwrap();
        assert_eq!(pkg.rev, "d4e8f1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8");
        assert_eq!(pkg.path, None);
        assert!(
            lock.find_package("quickfix", LockedPackageKind::Script)
                .is_some()
        );
        assert!(
            lock.find_package("quickfix", LockedPackageKind::Module)
                .is_none()
        );
        assert!(
            lock.find_package("nonexistent", LockedPackageKind::Module)
                .is_none()
        );
    }

    #[test]
    fn parse_spec_format() {
        let toml = r#"
# This file is generated automatically. Do not edit.
version = 1

[[package]]
name = "nu-git-utils"
git = "https://github.com/someuser/nu-git-utils"
tag = "v0.2.0"
rev = "d4e8f1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8"
sha256 = "abc123"
"#;
        let lock = Lockfile::from_str(toml).unwrap();
        assert_eq!(lock.version, 1);
        assert_eq!(lock.packages.len(), 1);
        assert_eq!(lock.packages[0].name, "nu-git-utils");
        assert_eq!(lock.packages[0].kind, LockedPackageKind::Module);
    }

    #[test]
    fn parse_script_format() {
        let toml = r#"
version = 1

[[package]]
name = "quickfix"
kind = "script"
git = "https://github.com/someuser/nu-scripts"
tag = "v0.5.0"
rev = "9a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b"
path = "scripts/quickfix.nu"
sha256 = "ghi789"
"#;
        let lock = Lockfile::from_str(toml).unwrap();
        assert_eq!(lock.packages[0].kind, LockedPackageKind::Script);
        assert_eq!(
            lock.packages[0].path.as_deref(),
            Some("scripts/quickfix.nu")
        );
    }
}
