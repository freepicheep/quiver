use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

use crate::error::{QuiverError, Result};

/// The top-level `nupackage.toml` manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub package: Package,
    #[serde(default, skip_serializing_if = "DependencyGroups::is_empty")]
    pub dependencies: DependencyGroups,
}

/// Dependency groups declared in `nupackage.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DependencyGroups {
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub modules: HashMap<String, DependencySpec>,
}

impl DependencyGroups {
    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }
}

/// The `[package]` section of a manifest.
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

/// A single module dependency specification from `[dependencies.modules]`.
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

impl DependencySpec {
    /// Validate that exactly one of tag/rev/branch is specified.
    pub fn validate(&self, name: &str) -> Result<()> {
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

    fn to_inline_toml(&self) -> Result<String> {
        let mut parts = vec![format!("git = {}", toml_scalar(&self.git)?)];
        if let Some(tag) = &self.tag {
            parts.push(format!("tag = {}", toml_scalar(tag)?));
        }
        if let Some(rev) = &self.rev {
            parts.push(format!("rev = {}", toml_scalar(rev)?));
        }
        if let Some(branch) = &self.branch {
            parts.push(format!("branch = {}", toml_scalar(branch)?));
        }
        Ok(parts.join(", "))
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
    /// Read and parse a `nupackage.toml` from the given directory.
    pub fn from_dir(dir: &Path) -> Result<Self> {
        let path = dir.join("nupackage.toml");
        if !path.exists() {
            return Err(QuiverError::NoManifest(dir.to_path_buf()));
        }
        let content = std::fs::read_to_string(&path)?;
        Self::from_str(&content)
    }

    /// Parse a manifest from a TOML string.
    pub fn from_str(s: &str) -> Result<Self> {
        let manifest: Manifest = toml::from_str(s)?;
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
        for (name, spec) in &self.dependencies.modules {
            spec.validate(name)?;
        }
        Ok(())
    }

    /// Serialize this manifest to a TOML string.
    pub fn to_toml_string(&self) -> Result<String> {
        let mut out = String::new();
        out.push_str("[package]\n");
        write_toml_field(&mut out, "name", &self.package.name)?;
        write_toml_field(&mut out, "version", &self.package.version)?;
        if let Some(description) = &self.package.description {
            write_toml_field(&mut out, "description", description)?;
        }
        if let Some(license) = &self.package.license {
            write_toml_field(&mut out, "license", license)?;
        }
        if let Some(authors) = &self.package.authors {
            write_toml_field(&mut out, "authors", authors)?;
        }
        if let Some(nu_version) = &self.package.nu_version {
            write_toml_field(&mut out, "nu-version", nu_version)?;
        }

        if !self.dependencies.modules.is_empty() {
            out.push_str("\n[dependencies.modules]\n");
            let mut modules: Vec<_> = self.dependencies.modules.iter().collect();
            modules.sort_by(|a, b| a.0.cmp(b.0));
            for (name, spec) in modules {
                out.push_str(&format!(
                    "{} = {{ {} }}\n",
                    bare_key_or_quoted(name),
                    spec.to_inline_toml()?
                ));
            }
        }

        Ok(out)
    }
}

fn write_toml_field<T: Serialize>(out: &mut String, key: &str, value: &T) -> Result<()> {
    out.push_str(key);
    out.push_str(" = ");
    out.push_str(&toml_scalar(value)?);
    out.push('\n');
    Ok(())
}

fn toml_scalar<T: Serialize>(value: &T) -> Result<String> {
    let serialized = toml::Value::try_from(value)?.to_string();
    Ok(serialized)
}

fn bare_key_or_quoted(key: &str) -> String {
    let is_bare = key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if is_bare {
        key.to_string()
    } else {
        format!("\"{}\"", key.replace('\"', "\\\""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn round_trip_parse_manifest() {
        let toml = r#"
[package]
name = "my-module"
version = "0.1.0"
description = "Useful utilities"

[dependencies.modules]
nu-utils = { git = "https://github.com/someuser/nu-utils", tag = "v1.2.3" }
nu-http = { git = "https://github.com/someuser/nu-http", branch = "main" }
"#;

        let manifest = Manifest::from_str(toml).unwrap();
        assert_eq!(manifest.package.name, "my-module");
        assert_eq!(manifest.package.version, "0.1.0");
        assert_eq!(manifest.dependencies.modules.len(), 2);
        assert!(manifest.dependencies.modules.contains_key("nu-utils"));
        assert!(manifest.dependencies.modules.contains_key("nu-http"));
    }

    #[test]
    fn reject_dep_with_multiple_refs() {
        let toml = r#"
[package]
name = "bad"
version = "0.1.0"

[dependencies.modules]
x = { git = "https://example.com/x", tag = "v1", branch = "main" }
"#;

        let err = Manifest::from_str(toml).unwrap_err();
        assert!(err.to_string().contains("specify only one"));
    }

    #[test]
    fn reject_dep_with_no_refs() {
        let toml = r#"
[package]
name = "bad"
version = "0.1.0"

[dependencies.modules]
x = { git = "https://example.com/x" }
"#;

        let err = Manifest::from_str(toml).unwrap_err();
        assert!(err.to_string().contains("must specify one"));
    }

    #[test]
    fn to_toml_string_emits_stable_sorted_dependencies() {
        let manifest = Manifest {
            package: Package {
                name: "my-module".to_string(),
                version: "0.1.0".to_string(),
                description: Some("Example module".to_string()),
                license: Some("MIT".to_string()),
                authors: Some(vec!["Alice".to_string(), "Bob".to_string()]),
                nu_version: Some("0.91".to_string()),
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
            },
        };

        let toml = manifest.to_toml_string().unwrap();

        assert!(toml.starts_with("[package]\n"));
        assert!(toml.contains("name = \"my-module\"\n"));
        assert!(toml.contains("version = \"0.1.0\"\n"));
        assert!(toml.contains("description = \"Example module\"\n"));
        assert!(toml.contains("license = \"MIT\"\n"));
        assert!(toml.contains("authors = [\"Alice\", \"Bob\"]\n"));
        assert!(toml.contains("nu-version = \"0.91\"\n"));

        let idx_modules = toml.find("[dependencies.modules]\n").unwrap();
        let idx_alpha = toml
            .find("alpha = { git = \"https://github.com/user/alpha\", branch = \"main\" }")
            .unwrap();
        let idx_zeta = toml
            .find("zeta = { git = \"https://github.com/user/zeta\", tag = \"v1.0.0\" }")
            .unwrap();
        assert!(idx_modules < idx_alpha && idx_alpha < idx_zeta);

        let parsed = Manifest::from_str(&toml).unwrap();
        assert_eq!(parsed.package.name, "my-module");
        assert_eq!(parsed.dependencies.modules.len(), 2);
    }
}
