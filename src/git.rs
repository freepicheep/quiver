use std::path::{Path, PathBuf};

use git2::{FetchOptions, RemoteCallbacks, Repository, build::RepoBuilder};

use crate::error::{QuiverError, Result};

/// Returns the global cache directory for git repos: `~/.cache/quiver/git/`.
pub fn cache_dir() -> Result<PathBuf> {
    let cache = dirs::cache_dir()
        .ok_or_else(|| QuiverError::Other("could not determine cache directory".to_string()))?;
    Ok(cache.join("quiver").join("git"))
}

/// Convert a git URL into a safe directory name for caching.
fn url_to_dirname(url: &str) -> String {
    url.replace("://", "_")
        .replace('/', "_")
        .replace('\\', "_")
        .replace('.', "_")
}

/// Clone a repository into the cache, or fetch updates if it already exists.
/// Returns the path to the cached repo.
pub fn clone_or_fetch(url: &str) -> Result<PathBuf> {
    let cache = cache_dir()?;
    std::fs::create_dir_all(&cache)?;

    let repo_dir = cache.join(url_to_dirname(url));

    if repo_dir.exists() {
        // Fetch latest from the remote
        let repo = Repository::open(&repo_dir)?;
        let mut remote = repo.find_remote("origin")?;
        let callbacks = RemoteCallbacks::new();
        let mut fetch_opts = FetchOptions::new();
        fetch_opts.remote_callbacks(callbacks);
        remote.fetch(&[] as &[&str], Some(&mut fetch_opts), None)?;
        Ok(repo_dir)
    } else {
        // Fresh clone
        let callbacks = RemoteCallbacks::new();
        let mut fetch_opts = FetchOptions::new();
        fetch_opts.remote_callbacks(callbacks);

        RepoBuilder::new()
            .fetch_options(fetch_opts)
            .clone(url, &repo_dir)?;

        Ok(repo_dir)
    }
}

/// Resolve a ref spec (tag, branch name, or commit SHA) to a full commit SHA string.
pub fn resolve_ref(repo_path: &Path, spec: &str, kind: RefKind) -> Result<String> {
    let repo = Repository::open(repo_path)?;

    match kind {
        RefKind::Rev => {
            // Direct commit SHA — validate it exists
            let oid = git2::Oid::from_str(spec)
                .map_err(|_| QuiverError::Other(format!("invalid commit SHA: {spec}")))?;
            let _commit = repo.find_commit(oid)?;
            Ok(spec.to_string())
        }
        RefKind::Tag => {
            // Try refs/tags/<spec> first, then the tag object itself
            let refname = format!("refs/tags/{spec}");
            let reference = repo.find_reference(&refname)?;
            let obj = reference.peel(git2::ObjectType::Commit)?;
            Ok(obj.id().to_string())
        }
        RefKind::Branch => {
            // Look up the remote tracking branch
            let refname = format!("refs/remotes/origin/{spec}");
            let reference = repo.find_reference(&refname)?;
            let obj = reference.peel(git2::ObjectType::Commit)?;
            Ok(obj.id().to_string())
        }
    }
}

/// Checkout a specific commit and export the working tree (without .git/) to `dest`.
pub fn export_to(repo_path: &Path, sha: &str, dest: &Path) -> Result<()> {
    let repo = Repository::open(repo_path)?;
    let oid = git2::Oid::from_str(sha)
        .map_err(|_| QuiverError::Other(format!("invalid commit SHA: {sha}")))?;
    let commit = repo.find_commit(oid)?;
    let tree = commit.tree()?;

    // Clean destination
    if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }
    std::fs::create_dir_all(dest)?;

    // Walk the tree and write files
    tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
        let name = match entry.name() {
            Some(n) => n,
            None => return git2::TreeWalkResult::Ok,
        };
        let path = dest.join(dir).join(name);

        match entry.kind() {
            Some(git2::ObjectType::Tree) => {
                let _ = std::fs::create_dir_all(&path);
            }
            Some(git2::ObjectType::Blob) => {
                if let Ok(obj) = repo.find_blob(entry.id()) {
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    let _ = std::fs::write(&path, obj.content());
                }
            }
            _ => {}
        }

        git2::TreeWalkResult::Ok
    })?;

    Ok(())
}

/// The kind of git ref being resolved.
#[derive(Debug, Clone, Copy)]
pub enum RefKind {
    Tag,
    Rev,
    Branch,
}

impl RefKind {
    /// Determine the ref kind from a dependency spec.
    pub fn from_spec(tag: &Option<String>, rev: &Option<String>, _branch: &Option<String>) -> Self {
        if rev.is_some() {
            RefKind::Rev
        } else if tag.is_some() {
            RefKind::Tag
        } else {
            RefKind::Branch
        }
    }
}

/// Find the latest tag in a cached repository.
///
/// Looks for tags matching common semver patterns (v1.2.3, 1.2.3, etc.)
/// and returns the most recent one. If no tags exist, returns `None`.
pub fn latest_tag(repo_path: &Path) -> Result<Option<String>> {
    let repo = Repository::open(repo_path)?;
    let mut tags: Vec<String> = Vec::new();

    repo.tag_foreach(|_oid, name| {
        if let Ok(name_str) = std::str::from_utf8(name) {
            if let Some(tag_name) = name_str.strip_prefix("refs/tags/") {
                tags.push(tag_name.to_string());
            }
        }
        true // continue iterating
    })?;

    if tags.is_empty() {
        return Ok(None);
    }

    // Sort tags — simple lexicographic on the numeric parts works for semver
    tags.sort();
    Ok(tags.last().cloned())
}

/// Extract a package name from a git URL.
///
/// e.g. `https://github.com/user/nu-utils` → `nu-utils`
///      `https://github.com/user/nu-utils.git` → `nu-utils`
pub fn repo_name_from_url(url: &str) -> Option<String> {
    let trimmed = url.trim_end_matches('/').trim_end_matches(".git");
    trimmed.rsplit('/').next().map(|s| s.to_string())
}

/// Detect the default branch of a cached repository (main, master, etc.)
pub fn default_branch(repo_path: &Path) -> Result<String> {
    let repo = Repository::open(repo_path)?;

    // Try common branch names
    for branch in &["main", "master"] {
        let refname = format!("refs/remotes/origin/{branch}");
        if repo.find_reference(&refname).is_ok() {
            return Ok(branch.to_string());
        }
    }

    Err(QuiverError::Other(
        "could not determine default branch".to_string(),
    ))
}
