use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Component, Path};

use crate::error::{NuanceError, Result};

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
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub scripts: HashMap<String, ScriptDependencySpec>,
}

impl DependencyGroups {
    pub fn is_empty(&self) -> bool {
        self.modules.is_empty() && self.scripts.is_empty()
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

/// A single script specification from `[dependencies.scripts]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptDependencySpec {
    pub git: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

impl ScriptDependencySpec {
    /// Validate path and that exactly one of tag/rev/branch is specified.
    pub fn validate(&self, name: &str) -> Result<()> {
        let rel = Path::new(&self.path);
        if self.path.trim().is_empty() || rel.as_os_str().is_empty() {
            return Err(NuanceError::Manifest(format!(
                "script dependency '{name}': path cannot be empty"
            )));
        }
        if rel.is_absolute()
            || rel.components().any(|c| {
                matches!(
                    c,
                    Component::ParentDir
                        | Component::RootDir
                        | Component::Prefix(_)
                        | Component::CurDir
                )
            })
        {
            return Err(NuanceError::Manifest(format!(
                "script dependency '{name}': path must be a relative repository path without '..'"
            )));
        }

        validate_ref_fields(
            name,
            "script dependency",
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
        let mut parts = vec![
            format!("git = {}", toml_scalar(&self.git)?),
            format!("path = {}", toml_scalar(&self.path)?),
        ];
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
        return Err(NuanceError::Manifest(format!(
            "{kind} '{name}': must specify one of 'tag', 'rev', or 'branch'"
        )));
    }
    if count > 1 {
        return Err(NuanceError::Manifest(format!(
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
            return Err(NuanceError::NoManifest(dir.to_path_buf()));
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
            return Err(NuanceError::Manifest(
                "package name cannot be empty".to_string(),
            ));
        }
        if self.package.version.is_empty() {
            return Err(NuanceError::Manifest(
                "package version cannot be empty".to_string(),
            ));
        }
        for (name, spec) in &self.dependencies.modules {
            spec.validate(name)?;
        }
        for (name, spec) in &self.dependencies.scripts {
            spec.validate(name)?;
        }
        for name in self.dependencies.modules.keys() {
            if self.dependencies.scripts.contains_key(name) {
                return Err(NuanceError::Manifest(format!(
                    "dependency '{name}' is declared as both a module and a script"
                )));
            }
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
                    toml_key(name),
                    spec.to_inline_toml()?
                ));
            }
        }

        if !self.dependencies.scripts.is_empty() {
            out.push_str("\n[dependencies.scripts]\n");
            let mut scripts: Vec<_> = self.dependencies.scripts.iter().collect();
            scripts.sort_by(|a, b| a.0.cmp(b.0));
            for (name, spec) in scripts {
                out.push_str(&format!(
                    "{} = {{ {} }}\n",
                    toml_key(name),
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
    Ok(toml::Value::try_from(value)?.to_string())
}

fn toml_key(key: &str) -> String {
    if !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        key.to_string()
    } else {
        toml::Value::String(key.to_string()).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_manifest() {
        let toml = r#"
[package]
name = "my-module"
version = "0.3.1"
description = "Useful utilities"
license = "MIT"
authors = ["Test Author"]
nu-version = ">=0.101.0"

[dependencies.modules]
nu-git-utils = { git = "https://github.com/someuser/nu-git-utils", tag = "v0.2.0" }
nu-str-extras = { git = "https://github.com/someuser/nu-str-extras", branch = "main" }

[dependencies.scripts]
quickfix = { git = "https://github.com/someuser/nu-scripts", path = "scripts/quickfix.nu", rev = "6d5eb9f" }
"#;
        let manifest = Manifest::from_str(toml).unwrap();
        assert_eq!(manifest.package.name, "my-module");
        assert_eq!(manifest.package.version, "0.3.1");
        assert_eq!(manifest.dependencies.modules.len(), 2);
        assert_eq!(manifest.dependencies.scripts.len(), 1);
        assert!(manifest.dependencies.modules.contains_key("nu-git-utils"));
        assert!(manifest.dependencies.modules.contains_key("nu-str-extras"));
        assert!(manifest.dependencies.scripts.contains_key("quickfix"));
    }

    #[test]
    fn parse_minimal_manifest() {
        let toml = r#"
[package]
name = "minimal"
version = "0.1.0"
"#;
        let manifest = Manifest::from_str(toml).unwrap();
        assert_eq!(manifest.package.name, "minimal");
        assert!(manifest.dependencies.is_empty());
    }

    #[test]
    fn reject_no_ref_spec() {
        let toml = r#"
[package]
name = "bad"
version = "0.1.0"

[dependencies.modules]
broken = { git = "https://github.com/user/broken" }
"#;
        let err = Manifest::from_str(toml).unwrap_err();
        assert!(err.to_string().contains("must specify one of"));
    }

    #[test]
    fn reject_multiple_ref_specs() {
        let toml = r#"
[package]
name = "bad"
version = "0.1.0"

[dependencies.modules]
broken = { git = "https://github.com/user/broken", tag = "v1", branch = "main" }
"#;
        let err = Manifest::from_str(toml).unwrap_err();
        assert!(err.to_string().contains("specify only one of"));
    }

    #[test]
    fn reject_script_with_invalid_path() {
        let toml = r#"
[package]
name = "bad"
version = "0.1.0"

[dependencies.scripts]
bad-script = { git = "https://github.com/user/scripts", path = "../evil.nu", tag = "v1.0.0" }
"#;
        let err = Manifest::from_str(toml).unwrap_err();
        assert!(
            err.to_string()
                .contains("path must be a relative repository path")
        );
    }

    #[test]
    fn reject_duplicate_name_across_modules_and_scripts() {
        let toml = r#"
[package]
name = "bad"
version = "0.1.0"

[dependencies.modules]
shared = { git = "https://github.com/user/module", tag = "v1.0.0" }

[dependencies.scripts]
shared = { git = "https://github.com/user/scripts", path = "scripts/shared.nu", tag = "v1.0.0" }
"#;
        let err = Manifest::from_str(toml).unwrap_err();
        assert!(
            err.to_string()
                .contains("declared as both a module and a script")
        );
    }

    #[test]
    fn serialize_uses_grouped_inline_dependency_tables() {
        let manifest = Manifest {
            package: Package {
                name: "my-module".to_string(),
                version: "0.1.0".to_string(),
                description: Some("Example module".to_string()),
                license: None,
                authors: None,
                nu_version: None,
            },
            dependencies: DependencyGroups {
                modules: HashMap::from([(
                    "nu-salesforce".to_string(),
                    DependencySpec {
                        git: "https://github.com/freepicheep/nu-salesforce".to_string(),
                        tag: Some("v0.1.0".to_string()),
                        rev: None,
                        branch: None,
                    },
                )]),
                scripts: HashMap::from([(
                    "quickfix".to_string(),
                    ScriptDependencySpec {
                        git: "https://github.com/user/nu-toolbox".to_string(),
                        path: "scripts/quickfix.nu".to_string(),
                        tag: None,
                        rev: Some("abc123".to_string()),
                        branch: None,
                    },
                )]),
            },
        };

        let toml = manifest.to_toml_string().unwrap();
        assert!(toml.contains("[dependencies.modules]\n"));
        assert!(toml.contains(
            "nu-salesforce = { git = \"https://github.com/freepicheep/nu-salesforce\", tag = \"v0.1.0\" }"
        ));
        assert!(toml.contains("[dependencies.scripts]\n"));
        assert!(toml.contains(
            "quickfix = { git = \"https://github.com/user/nu-toolbox\", path = \"scripts/quickfix.nu\", rev = \"abc123\" }"
        ));
        assert!(!toml.contains("[dependencies.modules.nu-salesforce]"));
    }

    #[test]
    fn reject_empty_name() {
        let toml = r#"
[package]
name = ""
version = "0.1.0"
"#;
        let err = Manifest::from_str(toml).unwrap_err();
        assert!(err.to_string().contains("name cannot be empty"));
    }
}
