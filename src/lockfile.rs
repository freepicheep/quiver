use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::path::Path;

use crate::error::{QuiverError, Result};
use crate::manifest::{nu_value_to_json, nuon_key, nuon_string};

/// The current lockfile schema version.
pub const LOCKFILE_VERSION: u32 = 2;

/// The `quiver.lock` lockfile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Lockfile {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nu: Option<LockedNu>,
    pub packages: Vec<LockedPackage>,
}

/// The pinned Nushell runtime for a project (present only when the manifest
/// declares a `nu-version`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LockedNu {
    /// The resolved, exact Nushell version (e.g. `0.107.0`).
    pub version: String,
    /// Per-target-triple download artifacts, pinned eagerly for every platform
    /// found in the release's checksums file.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub artifacts: BTreeMap<String, PlatformArtifact>,
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
    /// Source-tree checksum. Used for modules (platform-independent). Empty for
    /// plugins, whose per-platform hashes live in `artifacts`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sha256: String,
    /// Per-target-triple download artifacts (plugins only).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub artifacts: BTreeMap<String, PlatformArtifact>,
}

/// A platform-specific download artifact for a plugin or the nu runtime.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct PlatformArtifact {
    /// The release asset download URL for this platform.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset_url: Option<String>,
    /// SHA-256 of the downloaded release archive, verified against the
    /// release's signed checksums file. The cross-platform security anchor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset_sha256: Option<String>,
    /// SHA-256 of the extracted install directory on this platform. Optional;
    /// filled in by whichever machine installs here, for local cache-tamper
    /// detection and frozen re-verification of cached binaries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// The kind of installed artifact in the lockfile.
///
/// Unknown kinds are preserved for forward compatibility.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[serde(rename_all = "lowercase")]
pub enum LockedPackageKind {
    #[default]
    Module,
    Plugin,
    #[serde(other)]
    Other,
}

impl LockedPackageKind {
    fn is_module(kind: &LockedPackageKind) -> bool {
        matches!(kind, LockedPackageKind::Module)
    }
}

/// The target triple for the current platform, used as the key into
/// `artifacts` maps. Returns `None` on unsupported platforms.
pub fn current_target_triple() -> Option<String> {
    let triple = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("windows", "aarch64") => "aarch64-pc-windows-msvc",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        _ => return None,
    };
    Some(triple.to_string())
}

impl LockedPackage {
    /// Look up the artifact for a given target triple.
    pub fn find_artifact(&self, triple: &str) -> Option<&PlatformArtifact> {
        self.artifacts.get(triple)
    }

    /// Look up the artifact for the current platform.
    pub fn current_artifact(&self) -> Option<&PlatformArtifact> {
        current_target_triple().and_then(|triple| self.find_artifact(&triple))
    }
}

impl Lockfile {
    /// Read a lockfile from disk.
    pub fn from_path(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Self::from_str(&content)
    }

    /// Parse a lockfile from a NUON string.
    pub fn from_str(s: &str) -> Result<Self> {
        let value = nuon::from_nuon(s, None)?;
        let mut json = nu_value_to_json(value)?;
        migrate_legacy_lockfile_json(&mut json);
        serde_json::from_value(json)
            .map_err(|e| QuiverError::Lockfile(format!("invalid lockfile schema: {e}")))
    }

    /// Serialize the lockfile to a NUON string with the header comment.
    pub fn to_nuon_string(&self) -> String {
        let mut out = String::new();
        out.push_str("# This file is generated automatically. Do not edit.\n");
        out.push_str("{\n");
        out.push_str(&format!("  version: {},\n", self.version));
        if let Some(nu) = &self.nu {
            out.push_str("  nu: {\n");
            out.push_str(&format!("    version: {},\n", nuon_string(&nu.version)));
            if !nu.artifacts.is_empty() {
                out.push_str("    artifacts: {\n");
                for (triple, artifact) in &nu.artifacts {
                    out.push_str(&format!(
                        "      {}: {{ {} }},\n",
                        nuon_key(triple),
                        artifact_inline_nuon(artifact)
                    ));
                }
                out.push_str("    },\n");
            }
            out.push_str("  },\n");
        }
        out.push_str("  packages: [\n");
        for pkg in &self.packages {
            out.push_str("    {\n");
            out.push_str(&format!("      name: {},\n", nuon_string(&pkg.name)));
            if !LockedPackageKind::is_module(&pkg.kind) {
                let kind_str = match pkg.kind {
                    LockedPackageKind::Plugin => "plugin",
                    LockedPackageKind::Other => "other",
                    LockedPackageKind::Module => unreachable!(),
                };
                out.push_str(&format!("      kind: {},\n", nuon_string(kind_str)));
            }
            out.push_str(&format!("      git: {},\n", nuon_string(&pkg.git)));
            if let Some(tag) = &pkg.tag {
                out.push_str(&format!("      tag: {},\n", nuon_string(tag)));
            }
            out.push_str(&format!("      rev: {},\n", nuon_string(&pkg.rev)));
            if let Some(path) = &pkg.path {
                out.push_str(&format!(
                    "      {}: {},\n",
                    nuon_key("path"),
                    nuon_string(path)
                ));
            }
            if !pkg.sha256.is_empty() {
                out.push_str(&format!("      sha256: {},\n", nuon_string(&pkg.sha256)));
            }
            if !pkg.artifacts.is_empty() {
                out.push_str("      artifacts: {\n");
                for (triple, artifact) in &pkg.artifacts {
                    out.push_str(&format!(
                        "        {}: {{ {} }},\n",
                        nuon_key(triple),
                        artifact_inline_nuon(artifact)
                    ));
                }
                out.push_str("      },\n");
            }
            out.push_str("    },\n");
        }
        out.push_str("  ],\n");
        out.push_str("}\n");
        out
    }

    /// Write the lockfile to disk.
    pub fn write_to(&self, path: &Path) -> Result<()> {
        let content = self.to_nuon_string();
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

fn artifact_inline_nuon(artifact: &PlatformArtifact) -> String {
    let mut parts = Vec::new();
    if let Some(url) = &artifact.asset_url {
        parts.push(format!("asset_url: {}", nuon_string(url)));
    }
    if let Some(sha) = &artifact.asset_sha256 {
        parts.push(format!("asset_sha256: {}", nuon_string(sha)));
    }
    if let Some(sha) = &artifact.sha256 {
        parts.push(format!("sha256: {}", nuon_string(sha)));
    }
    parts.join(", ")
}

/// Migrate a parsed v1 lockfile (flat `asset_sha256`/`asset_url`/`sha256` on
/// plugins) into the v2 per-platform `artifacts` shape, in memory. The next
/// non-frozen install rewrites the file as v2.
fn migrate_legacy_lockfile_json(json: &mut JsonValue) {
    let Some(obj) = json.as_object_mut() else {
        return;
    };
    let version = obj.get("version").and_then(JsonValue::as_u64).unwrap_or(0);
    if version >= LOCKFILE_VERSION as u64 {
        return;
    }
    let Some(triple) = current_target_triple() else {
        return;
    };

    if let Some(JsonValue::Array(packages)) = obj.get_mut("packages") {
        for package in packages.iter_mut() {
            let Some(entry) = package.as_object_mut() else {
                continue;
            };
            let is_plugin = entry.get("kind").and_then(JsonValue::as_str) == Some("plugin");
            if !is_plugin {
                // Modules keep their platform-independent top-level sha256.
                continue;
            }
            if entry.contains_key("artifacts") {
                continue;
            }

            let mut artifact = serde_json::Map::new();
            for field in ["asset_url", "asset_sha256", "sha256"] {
                if let Some(value) = entry.remove(field)
                    && !value.is_null()
                {
                    artifact.insert(field.to_string(), value);
                }
            }
            if !artifact.is_empty() {
                let mut artifacts = serde_json::Map::new();
                artifacts.insert(triple.clone(), JsonValue::Object(artifact));
                entry.insert("artifacts".to_string(), JsonValue::Object(artifacts));
            }
        }
    }

    obj.insert(
        "version".to_string(),
        JsonValue::from(LOCKFILE_VERSION as u64),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact(url: &str, asset_sha: &str, sha: Option<&str>) -> PlatformArtifact {
        PlatformArtifact {
            asset_url: Some(url.to_string()),
            asset_sha256: Some(asset_sha.to_string()),
            sha256: sha.map(ToString::to_string),
        }
    }

    fn sample_lockfile() -> Lockfile {
        Lockfile {
            version: LOCKFILE_VERSION,
            nu: Some(LockedNu {
                version: "0.107.0".to_string(),
                artifacts: BTreeMap::from([
                    (
                        "aarch64-apple-darwin".to_string(),
                        artifact(
                            "https://example.com/nu-aarch64-apple-darwin.tar.gz",
                            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                            None,
                        ),
                    ),
                    (
                        "x86_64-unknown-linux-gnu".to_string(),
                        artifact(
                            "https://example.com/nu-x86_64-unknown-linux-gnu.tar.gz",
                            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                            None,
                        ),
                    ),
                ]),
            }),
            packages: vec![
                LockedPackage {
                    name: "nu-git-utils".to_string(),
                    kind: LockedPackageKind::Module,
                    git: "https://github.com/someuser/nu-git-utils".to_string(),
                    tag: Some("v0.2.0".to_string()),
                    rev: "d4e8f1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8".to_string(),
                    path: None,
                    sha256: "abc123".to_string(),
                    artifacts: BTreeMap::new(),
                },
                LockedPackage {
                    name: "nu_plugin_inc".to_string(),
                    kind: LockedPackageKind::Plugin,
                    git: "https://github.com/nushell/nu_plugin_inc".to_string(),
                    tag: Some("v0.91.0".to_string()),
                    rev: "2a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b".to_string(),
                    path: Some("nu_plugin_inc".to_string()),
                    sha256: String::new(),
                    artifacts: BTreeMap::from([
                        (
                            "aarch64-apple-darwin".to_string(),
                            artifact(
                                "https://github.com/nushell/nu_plugin_inc/releases/download/v0.91.0/nu_plugin_inc-aarch64-apple-darwin.tar.gz",
                                "1111111111111111111111111111111111111111111111111111111111111111",
                                Some(
                                    "cafebabecafebabecafebabecafebabecafebabecafebabecafebabecafebabe",
                                ),
                            ),
                        ),
                        (
                            "x86_64-unknown-linux-gnu".to_string(),
                            artifact(
                                "https://github.com/nushell/nu_plugin_inc/releases/download/v0.91.0/nu_plugin_inc-x86_64-unknown-linux-gnu.tar.gz",
                                "2222222222222222222222222222222222222222222222222222222222222222",
                                None,
                            ),
                        ),
                    ]),
                },
            ],
        }
    }

    #[test]
    fn round_trip() {
        let lock = sample_lockfile();
        let serialized = lock.to_nuon_string();
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
        assert!(pkg.artifacts.is_empty());
        assert!(
            lock.find_package("nonexistent", LockedPackageKind::Module)
                .is_none()
        );
    }

    #[test]
    fn find_artifact_by_triple() {
        let lock = sample_lockfile();
        let plugin = lock
            .find_package("nu_plugin_inc", LockedPackageKind::Plugin)
            .unwrap();
        let artifact = plugin.find_artifact("aarch64-apple-darwin").unwrap();
        assert_eq!(
            artifact.asset_sha256.as_deref(),
            Some("1111111111111111111111111111111111111111111111111111111111111111")
        );
        assert!(plugin.find_artifact("nonexistent-triple").is_none());
    }

    #[test]
    fn parse_spec_format() {
        let nuon = r#"
# This file is generated automatically. Do not edit.
{
  version: 2,
  packages: [
    {
      name: "nu-git-utils",
      git: "https://github.com/someuser/nu-git-utils",
      tag: "v0.2.0",
      rev: "d4e8f1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8",
      sha256: "abc123",
    },
  ],
}
"#;
        let lock = Lockfile::from_str(nuon).unwrap();
        assert_eq!(lock.version, 2);
        assert_eq!(lock.packages.len(), 1);
        assert_eq!(lock.packages[0].name, "nu-git-utils");
        assert_eq!(lock.packages[0].kind, LockedPackageKind::Module);
        assert!(lock.nu.is_none());
    }

    #[test]
    fn parse_nu_pin() {
        let nuon = r#"
{
  version: 2,
  nu: {
    version: "0.107.0",
    artifacts: {
      x86_64-unknown-linux-gnu: { asset_url: "https://example.com/nu.tar.gz", asset_sha256: "deadbeef" },
    },
  },
  packages: [],
}
"#;
        let lock = Lockfile::from_str(nuon).unwrap();
        let nu = lock.nu.unwrap();
        assert_eq!(nu.version, "0.107.0");
        assert_eq!(
            nu.artifacts["x86_64-unknown-linux-gnu"]
                .asset_url
                .as_deref(),
            Some("https://example.com/nu.tar.gz")
        );
    }

    #[test]
    fn parse_plugin_artifacts() {
        let nuon = r#"
{
  version: 2,
  packages: [
    {
      name: "nu_plugin_inc",
      kind: "plugin",
      git: "https://github.com/nushell/nu_plugin_inc",
      tag: "v0.91.0",
      rev: "9a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b",
      path: "nu_plugin_inc",
      artifacts: {
        aarch64-apple-darwin: { asset_url: "https://example.com/a.tar.gz", asset_sha256: "aaaa", sha256: "bbbb" },
      },
    },
  ],
}
"#;
        let lock = Lockfile::from_str(nuon).unwrap();
        assert_eq!(lock.packages[0].kind, LockedPackageKind::Plugin);
        let artifact = lock.packages[0]
            .find_artifact("aarch64-apple-darwin")
            .unwrap();
        assert_eq!(artifact.asset_sha256.as_deref(), Some("aaaa"));
        assert_eq!(artifact.sha256.as_deref(), Some("bbbb"));
    }

    #[test]
    fn migrate_v1_plugin_into_artifacts() {
        let nuon = r#"
{
  version: 1,
  packages: [
    {
      name: "nu_plugin_inc",
      kind: "plugin",
      git: "https://github.com/nushell/nu_plugin_inc",
      tag: "v0.91.0",
      rev: "9a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b",
      path: "nu_plugin_inc",
      sha256: "extractedhash",
      asset_sha256: "archivehash",
      asset_url: "https://example.com/plugin.tar.gz",
    },
  ],
}
"#;
        let lock = Lockfile::from_str(nuon).unwrap();
        assert_eq!(lock.version, 2);
        let plugin = &lock.packages[0];
        // Flat fields are gone; per-platform artifact filled for current triple.
        assert!(plugin.sha256.is_empty());
        let triple = current_target_triple().expect("supported test platform");
        let artifact = plugin.find_artifact(&triple).unwrap();
        assert_eq!(artifact.asset_sha256.as_deref(), Some("archivehash"));
        assert_eq!(
            artifact.asset_url.as_deref(),
            Some("https://example.com/plugin.tar.gz")
        );
        assert_eq!(artifact.sha256.as_deref(), Some("extractedhash"));
    }

    #[test]
    fn migrate_v1_module_keeps_sha256() {
        let nuon = r#"
{
  version: 1,
  packages: [
    {
      name: "nu-git-utils",
      git: "https://github.com/someuser/nu-git-utils",
      tag: "v0.2.0",
      rev: "d4e8f1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8",
      sha256: "moduletreehash",
    },
  ],
}
"#;
        let lock = Lockfile::from_str(nuon).unwrap();
        assert_eq!(lock.version, 2);
        assert_eq!(lock.packages[0].sha256, "moduletreehash");
        assert!(lock.packages[0].artifacts.is_empty());
    }

    #[test]
    fn parse_unknown_kind_as_other() {
        let nuon = r#"
{
  version: 2,
  packages: [
    {
      name: "future-artifact",
      kind: "futurekind",
      git: "https://github.com/someuser/future",
      tag: "v0.5.0",
      rev: "9a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b",
      sha256: "ghi789",
    },
  ],
}
"#;
        let lock = Lockfile::from_str(nuon).unwrap();
        assert_eq!(lock.packages[0].kind, LockedPackageKind::Other);
    }
}
