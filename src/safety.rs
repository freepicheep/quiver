use std::path::{Component, Path, PathBuf};

use crate::error::{QuiverError, Result};

pub fn validate_dependency_name(name: &str, kind: &str) -> Result<()> {
    if name.trim().is_empty() {
        return Err(QuiverError::Manifest(format!(
            "{kind} name cannot be empty"
        )));
    }
    if name != name.trim() {
        return Err(QuiverError::Manifest(format!(
            "{kind} name '{name}' cannot have leading or trailing whitespace"
        )));
    }
    if name == "." || name == ".." {
        return Err(QuiverError::Manifest(format!(
            "{kind} name '{name}' is not allowed"
        )));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(QuiverError::Manifest(format!(
            "{kind} name '{name}' cannot contain path separators"
        )));
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.')
    {
        return Err(QuiverError::Manifest(format!(
            "{kind} name '{name}' contains invalid characters; allowed: A-Z a-z 0-9 . _ -"
        )));
    }
    Ok(())
}

pub fn validate_binary_name(name: &str, context: &str) -> Result<()> {
    if name.trim().is_empty() {
        return Err(QuiverError::Manifest(format!("{context} cannot be empty")));
    }
    if name != name.trim() {
        return Err(QuiverError::Manifest(format!(
            "{context} '{name}' cannot have leading or trailing whitespace"
        )));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(QuiverError::Manifest(format!(
            "{context} '{name}' cannot contain path separators"
        )));
    }
    if name == "." || name == ".." {
        return Err(QuiverError::Manifest(format!(
            "{context} '{name}' is not allowed"
        )));
    }
    Ok(())
}

pub fn validate_secure_git_source(source: &str, context: &str) -> Result<()> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Err(QuiverError::Manifest(format!("{context} cannot be empty")));
    }

    if trimmed.starts_with("http://") {
        return Err(QuiverError::Manifest(format!(
            "{context} '{source}' uses insecure HTTP; use HTTPS or SSH (git@...)"
        )));
    }

    if trimmed.starts_with("https://")
        || trimmed.starts_with("git@")
        || trimmed.starts_with("ssh://")
    {
        return Ok(());
    }

    Err(QuiverError::Manifest(format!(
        "{context} '{source}' must use HTTPS or SSH (git@...)"
    )))
}

pub fn normalized_relative_path(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(segment) => normalized.push(segment),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependency_name_rejects_path_traversal_patterns() {
        assert!(validate_dependency_name("../evil", "dependency").is_err());
        assert!(validate_dependency_name("..", "dependency").is_err());
        assert!(validate_dependency_name("a/b", "dependency").is_err());
        assert!(validate_dependency_name("a\\b", "dependency").is_err());
    }

    #[test]
    fn dependency_name_accepts_common_safe_names() {
        assert!(validate_dependency_name("nu-utils", "dependency").is_ok());
        assert!(validate_dependency_name("nu_plugin_query", "dependency").is_ok());
        assert!(validate_dependency_name("nu.plugin", "dependency").is_ok());
    }

    #[test]
    fn secure_git_source_allows_https_and_ssh() {
        assert!(validate_secure_git_source("https://github.com/org/repo", "git").is_ok());
        assert!(validate_secure_git_source("git@github.com:org/repo.git", "git").is_ok());
        assert!(validate_secure_git_source("ssh://git@github.com/org/repo.git", "git").is_ok());
    }

    #[test]
    fn secure_git_source_rejects_http_and_unknown_schemes() {
        assert!(validate_secure_git_source("http://github.com/org/repo", "git").is_err());
        assert!(validate_secure_git_source("git://github.com/org/repo", "git").is_err());
    }

    #[test]
    fn normalized_relative_path_blocks_absolute_and_parent_components() {
        assert!(normalized_relative_path(Path::new("../x")).is_none());
        assert!(normalized_relative_path(Path::new("/tmp/x")).is_none());
        assert_eq!(
            normalized_relative_path(Path::new("./nested/mod.nu")).unwrap(),
            PathBuf::from("nested/mod.nu")
        );
    }
}
