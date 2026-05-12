use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as JsonValue};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{QuiverError, Result};
use crate::nu;
use crate::safety;

pub const MANIFEST_FILE_NAME: &str = "nupackage.nuon";

/// The top-level `nupackage.nuon` manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub package: Package,
    #[serde(default, skip_serializing_if = "DependencyGroups::is_empty")]
    pub dependencies: DependencyGroups,
}

/// Dependency groups declared in `nupackage.nuon`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DependencyGroups {
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub modules: HashMap<String, DependencySpec>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub plugins: HashMap<String, PluginDependencySpec>,
}

impl DependencyGroups {
    pub fn is_empty(&self) -> bool {
        self.modules.is_empty() && self.plugins.is_empty()
    }
}

/// The `package` section of a manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Package {
    pub name: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authors: Option<Vec<String>>,
    #[serde(rename = "nu-version", skip_serializing_if = "Option::is_none")]
    pub nu_version: Option<String>,
}

/// A single module dependency specification from `dependencies.modules`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencySpec {
    pub git: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// A single plugin dependency specification from `dependencies.plugins`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginDependencySpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default)]
    pub git: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bin: Option<String>,
}

impl DependencySpec {
    /// Validate that exactly one of tag/rev/branch is specified.
    pub fn validate(&self, name: &str) -> Result<()> {
        safety::validate_secure_git_source(&self.git, "module dependency git source")?;
        validate_ref_fields(
            name,
            "module dependency",
            &self.tag,
            &self.rev,
            &self.branch,
        )
    }

    /// Returns the git ref string (tag, rev, or branch value).
    pub fn ref_spec(&self) -> &str {
        self.rev
            .as_deref()
            .or(self.tag.as_deref())
            .or(self.branch.as_deref())
            .expect("validated: one of tag/rev/branch is set")
    }

    pub(crate) fn to_inline_nuon(&self) -> String {
        let mut parts = vec![format!("git: {}", nuon_string(&self.git))];
        if let Some(tag) = &self.tag {
            parts.push(format!("tag: {}", nuon_string(tag)));
        }
        if let Some(rev) = &self.rev {
            parts.push(format!("rev: {}", nuon_string(rev)));
        }
        if let Some(branch) = &self.branch {
            parts.push(format!("branch: {}", nuon_string(branch)));
        }
        parts.join(", ")
    }
}

impl PluginDependencySpec {
    /// Validate plugin dependency source and ref requirements.
    pub fn validate(&self, name: &str) -> Result<()> {
        let source = self.source.as_deref().unwrap_or("git");
        if !matches!(source, "git" | "nu-core") {
            return Err(QuiverError::Manifest(format!(
                "plugin dependency '{name}': unsupported source '{source}' (expected 'git' or 'nu-core')"
            )));
        }

        if source == "nu-core" {
            if !self.git.trim().is_empty() {
                return Err(QuiverError::Manifest(format!(
                    "plugin dependency '{name}': git is not allowed when source = 'nu-core'"
                )));
            }
            if self.tag.is_some() || self.rev.is_some() || self.branch.is_some() {
                return Err(QuiverError::Manifest(format!(
                    "plugin dependency '{name}': tag/rev/branch are not allowed when source = 'nu-core'"
                )));
            }
            if self.bin.as_deref().is_some_and(|bin| bin.trim().is_empty()) {
                return Err(QuiverError::Manifest(format!(
                    "plugin dependency '{name}': bin cannot be empty when set"
                )));
            }
            return Ok(());
        }

        validate_ref_fields(
            name,
            "plugin dependency",
            &self.tag,
            &self.rev,
            &self.branch,
        )?;

        if self.git.trim().is_empty() {
            return Err(QuiverError::Manifest(format!(
                "plugin dependency '{name}': git cannot be empty"
            )));
        }
        safety::validate_secure_git_source(&self.git, "plugin dependency git source")?;
        if self.bin.as_deref().is_some_and(|bin| bin.trim().is_empty()) {
            return Err(QuiverError::Manifest(format!(
                "plugin dependency '{name}': bin cannot be empty when set"
            )));
        }

        Ok(())
    }

    /// Returns the git ref string (tag, rev, or branch value).
    pub fn ref_spec(&self) -> &str {
        self.rev
            .as_deref()
            .or(self.tag.as_deref())
            .or(self.branch.as_deref())
            .expect("validated: one of tag/rev/branch is set")
    }

    pub(crate) fn to_inline_nuon(&self) -> String {
        let source = self.source.as_deref().unwrap_or("git");
        let mut parts = Vec::new();
        if self.source.is_some() {
            parts.push(format!("source: {}", nuon_string(&source)));
        }
        if source != "nu-core" {
            parts.push(format!("git: {}", nuon_string(&self.git)));
        }
        if let Some(tag) = &self.tag {
            parts.push(format!("tag: {}", nuon_string(tag)));
        }
        if let Some(rev) = &self.rev {
            parts.push(format!("rev: {}", nuon_string(rev)));
        }
        if let Some(branch) = &self.branch {
            parts.push(format!("branch: {}", nuon_string(branch)));
        }
        if let Some(bin) = &self.bin {
            parts.push(format!("bin: {}", nuon_string(bin)));
        }
        parts.join(", ")
    }
}

fn validate_ref_fields(
    name: &str,
    kind: &str,
    tag: &Option<String>,
    rev: &Option<String>,
    branch: &Option<String>,
) -> Result<()> {
    let count = [tag, rev, branch].iter().filter(|v| v.is_some()).count();

    if count == 0 {
        return Err(QuiverError::Manifest(format!(
            "{kind} '{name}': must specify one of 'tag', 'rev', or 'branch'"
        )));
    }
    if count > 1 {
        return Err(QuiverError::Manifest(format!(
            "{kind} '{name}': specify only one of 'tag', 'rev', or 'branch'"
        )));
    }
    Ok(())
}

impl Manifest {
    /// Find the nearest ancestor directory containing `nupackage.nuon`.
    pub fn find_project_dir(start: &Path) -> Option<PathBuf> {
        start
            .ancestors()
            .find(|dir| dir.join(MANIFEST_FILE_NAME).exists())
            .map(Path::to_path_buf)
    }

    /// Read and parse a `nupackage.nuon` from the given directory.
    pub fn from_dir(dir: &Path) -> Result<Self> {
        let path = dir.join(MANIFEST_FILE_NAME);
        if !path.exists() {
            return Err(QuiverError::NoManifest(dir.to_path_buf()));
        }
        let content = std::fs::read_to_string(&path)?;
        Self::from_str(&content)
    }

    /// Parse a manifest from a NUON string.
    pub fn from_str(s: &str) -> Result<Self> {
        let value = nuon::from_nuon(s, None)?;
        let json = nu_value_to_json(value)?;
        let manifest: Manifest = serde_json::from_value(json)
            .map_err(|err| QuiverError::Manifest(format!("invalid manifest schema: {err}")))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Validate the manifest contents.
    fn validate(&self) -> Result<()> {
        if self.package.name.is_empty() {
            return Err(QuiverError::Manifest(
                "package name cannot be empty".to_string(),
            ));
        }
        if self.package.version.is_empty() {
            return Err(QuiverError::Manifest(
                "package version cannot be empty".to_string(),
            ));
        }
        if let Some(nu_version) = &self.package.nu_version {
            nu::parse_nu_version_requirement(nu_version).map_err(|err| {
                QuiverError::Manifest(format!(
                    "package nu-version '{nu_version}' is invalid: {err}"
                ))
            })?;
        }
        for (name, spec) in &self.dependencies.modules {
            safety::validate_dependency_name(name, "module dependency")?;
            spec.validate(name)?;
        }
        for (name, spec) in &self.dependencies.plugins {
            safety::validate_dependency_name(name, "plugin dependency")?;
            spec.validate(name)?;
            if let Some(bin) = spec.bin.as_deref() {
                safety::validate_binary_name(bin, "plugin dependency bin")?;
            }
        }
        Ok(())
    }

    /// Serialize this manifest to a NUON string.
    pub fn to_nuon_string(&self) -> String {
        let mut out = String::new();
        out.push_str("{\n");
        out.push_str("  package: {\n");
        write_nuon_field(&mut out, 4, "name", &self.package.name);
        write_nuon_field(&mut out, 4, "version", &self.package.version);
        if let Some(description) = &self.package.description {
            write_nuon_field(&mut out, 4, "description", description);
        }
        if let Some(license) = &self.package.license {
            write_nuon_field(&mut out, 4, "license", license);
        }
        if let Some(authors) = &self.package.authors {
            write_nuon_list_field(&mut out, 4, "authors", authors);
        }
        if let Some(nu_version) = &self.package.nu_version {
            write_nuon_field(&mut out, 4, "nu-version", nu_version);
        }
        out.push_str("  },\n");

        if !self.dependencies.is_empty() {
            out.push_str("  dependencies: {\n");
        }

        if !self.dependencies.modules.is_empty() {
            out.push_str("    modules: {\n");
            let mut modules: Vec<_> = self.dependencies.modules.iter().collect();
            modules.sort_by(|a, b| a.0.cmp(b.0));
            for (name, spec) in modules {
                out.push_str(&format!(
                    "      {}: {{ {} }},\n",
                    nuon_key(name),
                    spec.to_inline_nuon()
                ));
            }
            out.push_str("    },\n");
        }

        if !self.dependencies.plugins.is_empty() {
            out.push_str("    plugins: {\n");
            let mut plugins: Vec<_> = self.dependencies.plugins.iter().collect();
            plugins.sort_by(|a, b| a.0.cmp(b.0));
            for (name, spec) in plugins {
                out.push_str(&format!(
                    "      {}: {{ {} }},\n",
                    nuon_key(name),
                    spec.to_inline_nuon()
                ));
            }
            out.push_str("    },\n");
        }

        if !self.dependencies.is_empty() {
            out.push_str("  },\n");
        }
        out.push_str("}\n");
        out
    }
}

fn write_nuon_field(out: &mut String, indent: usize, key: &str, value: &str) {
    out.push_str(&" ".repeat(indent));
    out.push_str(&nuon_key(key));
    out.push_str(": ");
    out.push_str(&nuon_string(value));
    out.push_str(",\n");
}

fn write_nuon_list_field(out: &mut String, indent: usize, key: &str, values: &[String]) {
    out.push_str(&" ".repeat(indent));
    out.push_str(&nuon_key(key));
    out.push_str(": [");
    for (idx, value) in values.iter().enumerate() {
        if idx > 0 {
            out.push_str(", ");
        }
        out.push_str(&nuon_string(value));
    }
    out.push_str("],\n");
}

pub(crate) fn nuon_key(key: &str) -> String {
    let is_bare = key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if is_bare {
        key.to_string()
    } else {
        nuon_string(key)
    }
}

pub(crate) fn nuon_string(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string cannot fail")
}

pub(crate) fn nu_value_to_json(value: nu_protocol::Value) -> Result<JsonValue> {
    match value {
        nu_protocol::Value::Bool { val, .. } => Ok(JsonValue::Bool(val)),
        nu_protocol::Value::Int { val, .. } => Ok(JsonValue::Number(val.into())),
        nu_protocol::Value::Float { val, .. } => serde_json::Number::from_f64(val)
            .map(JsonValue::Number)
            .ok_or_else(|| {
                QuiverError::Manifest("manifest contains a non-finite float".to_string())
            }),
        nu_protocol::Value::String { val, .. } | nu_protocol::Value::Glob { val, .. } => {
            Ok(JsonValue::String(val))
        }
        nu_protocol::Value::List { vals, .. } => vals
            .into_iter()
            .map(nu_value_to_json)
            .collect::<Result<Vec<_>>>()
            .map(JsonValue::Array),
        nu_protocol::Value::Record { val, .. } => {
            let mut object = Map::new();
            for (key, value) in val.into_owned().into_iter() {
                object.insert(key, nu_value_to_json(value)?);
            }
            Ok(JsonValue::Object(object))
        }
        nu_protocol::Value::Nothing { .. } => Ok(JsonValue::Null),
        other => Err(QuiverError::Manifest(format!(
            "unsupported NUON value in manifest: {}",
            other.get_type()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("quiver_manifest_{prefix}_{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn round_trip_parse_manifest() {
        let nuon = r#"
{
  package: {
    name: "my-module",
    version: "0.1.0",
    description: "Useful utilities",
  },
  dependencies: {
    modules: {
      nu-utils: { git: "https://github.com/someuser/nu-utils", tag: "v1.2.3" },
      nu-http: { git: "https://github.com/someuser/nu-http", branch: "main" },
    },
    plugins: {
      nu_plugin_inc: { git: "https://github.com/someuser/nu_plugin_inc", tag: "v0.8.0", bin: "nu_plugin_inc" },
    },
  },
}
"#;

        let manifest = Manifest::from_str(nuon).unwrap();
        assert_eq!(manifest.package.name, "my-module");
        assert_eq!(manifest.package.version, "0.1.0");
        assert_eq!(manifest.dependencies.modules.len(), 2);
        assert_eq!(manifest.dependencies.plugins.len(), 1);
        assert!(manifest.dependencies.modules.contains_key("nu-utils"));
        assert!(manifest.dependencies.modules.contains_key("nu-http"));
        assert!(manifest.dependencies.plugins.contains_key("nu_plugin_inc"));
    }

    #[test]
    fn reject_dep_with_multiple_refs() {
        let nuon = r#"
{
  package: { name: "bad", version: "0.1.0" },
  dependencies: { modules: { x: { git: "https://example.com/x", tag: "v1", branch: "main" } } },
}
"#;

        let err = Manifest::from_str(nuon).unwrap_err();
        assert!(err.to_string().contains("specify only one"));
    }

    #[test]
    fn reject_dep_with_no_refs() {
        let nuon = r#"
{
  package: { name: "bad", version: "0.1.0" },
  dependencies: { modules: { x: { git: "https://example.com/x" } } },
}
"#;

        let err = Manifest::from_str(nuon).unwrap_err();
        assert!(err.to_string().contains("must specify one"));
    }

    #[test]
    fn reject_module_dependency_with_insecure_http_git_source() {
        let nuon = r#"
{
  package: { name: "bad", version: "0.1.0" },
  dependencies: { modules: { x: { git: "http://example.com/x", tag: "v1.0.0" } } },
}
"#;

        let err = Manifest::from_str(nuon).unwrap_err();
        assert!(err.to_string().contains("insecure HTTP"));
    }

    #[test]
    fn to_nuon_string_emits_stable_sorted_dependencies() {
        let manifest = Manifest {
            package: Package {
                name: "my-module".to_string(),
                version: "0.1.0".to_string(),
                description: Some("Example module".to_string()),
                license: Some("MIT".to_string()),
                authors: Some(vec!["Alice".to_string(), "Bob".to_string()]),
                nu_version: Some("0.91.0".to_string()),
            },
            dependencies: DependencyGroups {
                modules: HashMap::from([
                    (
                        "zeta".to_string(),
                        DependencySpec {
                            git: "https://github.com/user/zeta".to_string(),
                            tag: Some("v1.0.0".to_string()),
                            rev: None,
                            branch: None,
                        },
                    ),
                    (
                        "alpha".to_string(),
                        DependencySpec {
                            git: "https://github.com/user/alpha".to_string(),
                            tag: None,
                            rev: None,
                            branch: Some("main".to_string()),
                        },
                    ),
                ]),
                plugins: HashMap::from([(
                    "nu_plugin_inc".to_string(),
                    PluginDependencySpec {
                        source: None,
                        git: "https://github.com/user/nu_plugin_inc".to_string(),
                        tag: Some("v0.9.0".to_string()),
                        rev: None,
                        branch: None,
                        bin: Some("nu_plugin_inc".to_string()),
                    },
                )]),
            },
        };

        let nuon = manifest.to_nuon_string();

        assert!(nuon.starts_with("{\n  package: {\n"));
        assert!(nuon.contains("name: \"my-module\",\n"));
        assert!(nuon.contains("version: \"0.1.0\",\n"));
        assert!(nuon.contains("description: \"Example module\",\n"));
        assert!(nuon.contains("license: \"MIT\",\n"));
        assert!(nuon.contains("authors: [\"Alice\", \"Bob\"],\n"));
        assert!(nuon.contains("nu-version: \"0.91.0\",\n"));

        let idx_modules = nuon.find("modules: {\n").unwrap();
        let idx_plugins = nuon.find("plugins: {\n").unwrap();
        let idx_alpha = nuon
            .find("alpha: { git: \"https://github.com/user/alpha\", branch: \"main\" }")
            .unwrap();
        let idx_zeta = nuon
            .find("zeta: { git: \"https://github.com/user/zeta\", tag: \"v1.0.0\" }")
            .unwrap();
        let idx_plugin = nuon
            .find(
                "nu_plugin_inc: { git: \"https://github.com/user/nu_plugin_inc\", tag: \"v0.9.0\", bin: \"nu_plugin_inc\" }",
            )
            .unwrap();
        assert!(idx_modules < idx_alpha && idx_alpha < idx_zeta);
        assert!(idx_plugins < idx_plugin);

        let parsed = Manifest::from_str(&nuon).unwrap();
        assert_eq!(parsed.package.name, "my-module");
        assert_eq!(parsed.dependencies.modules.len(), 2);
        assert_eq!(parsed.dependencies.plugins.len(), 1);
    }

    #[test]
    fn reject_invalid_nu_version_requirement() {
        let nuon = r#"{ package: { name: "bad", version: "0.1.0", nu-version: "not-semver" } }"#;

        let err = Manifest::from_str(nuon).unwrap_err();
        assert!(err.to_string().contains("nu-version"));
    }

    #[test]
    fn accepts_semver_range_nu_version_requirement() {
        let nuon =
            r#"{ package: { name: "ok", version: "0.1.0", nu-version: ">=0.110.0, <0.112.0" } }"#;

        let manifest = Manifest::from_str(nuon).unwrap();
        assert_eq!(
            manifest.package.nu_version.as_deref(),
            Some(">=0.110.0, <0.112.0")
        );
    }

    #[test]
    fn reject_plugin_dep_with_no_refs() {
        let nuon = r#"
{
  package: { name: "bad", version: "0.1.0" },
  dependencies: { plugins: { x: { git: "https://example.com/x", bin: "x" } } },
}
"#;

        let err = Manifest::from_str(nuon).unwrap_err();
        assert!(err.to_string().contains("must specify one"));
    }

    #[test]
    fn reject_plugin_dep_with_empty_bin() {
        let nuon = r#"
{
  package: { name: "bad", version: "0.1.0" },
  dependencies: { plugins: { x: { git: "https://example.com/x", tag: "v1.0.0", bin: "   " } } },
}
"#;

        let err = Manifest::from_str(nuon).unwrap_err();
        assert!(err.to_string().contains("bin cannot be empty"));
    }

    #[test]
    fn reject_plugin_dependency_with_insecure_http_git_source() {
        let nuon = r#"
{
  package: { name: "bad", version: "0.1.0" },
  dependencies: { plugins: { x: { git: "http://example.com/x", tag: "v1.0.0", bin: "x" } } },
}
"#;

        let err = Manifest::from_str(nuon).unwrap_err();
        assert!(err.to_string().contains("insecure HTTP"));
    }

    #[test]
    fn accepts_core_plugin_source_without_git_or_refs() {
        let nuon = r#"
{
  package: { name: "ok", version: "0.1.0" },
  dependencies: { plugins: { nu_plugin_polars: { source: "nu-core", bin: "nu_plugin_polars" } } },
}
"#;

        let manifest = Manifest::from_str(nuon).unwrap();
        let plugin = manifest
            .dependencies
            .plugins
            .get("nu_plugin_polars")
            .unwrap();
        assert_eq!(plugin.source.as_deref(), Some("nu-core"));
        assert!(plugin.git.is_empty());
        assert!(plugin.tag.is_none());
        assert!(plugin.rev.is_none());
        assert!(plugin.branch.is_none());
    }

    #[test]
    fn reject_core_plugin_source_with_git_or_refs() {
        let nuon = r#"
{
  package: { name: "bad", version: "0.1.0" },
  dependencies: { plugins: { nu_plugin_polars: { source: "nu-core", git: "https://example.com/x", tag: "v1.0.0" } } },
}
"#;

        let err = Manifest::from_str(nuon).unwrap_err();
        assert!(err.to_string().contains("source = 'nu-core'"));
    }

    #[test]
    fn find_project_dir_walks_up_parent_directories() {
        let root = make_temp_dir("find_project_dir");
        let nested = root.join("src").join("commands");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            root.join("nupackage.nuon"),
            r#"{ package: { name: "demo", version: "0.1.0" } }"#,
        )
        .unwrap();

        let found = Manifest::find_project_dir(&nested);

        assert_eq!(found.as_deref(), Some(root.as_path()));

        let _ = std::fs::remove_dir_all(root);
    }
}
