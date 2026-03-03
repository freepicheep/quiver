use std::path::{Path, PathBuf};

use git2::{FetchOptions, Progress, RemoteCallbacks, Repository, build::RepoBuilder};
use indicatif::ProgressBar;
use sha2::{Digest, Sha256};

use crate::config;
use crate::error::{QuiverError, Result};
use crate::ui;

/// Returns the global install directory for git repos:
/// `~/.local/share/quiver/installs/git/` on macOS/Linux.
pub fn cache_dir() -> Result<PathBuf> {
    Ok(config::installs_root_dir()?.join("git"))
}

/// Convert a git URL into a safe directory name for caching.
fn url_to_dirname(url: &str) -> String {
    let digest = Sha256::digest(url.as_bytes());
    format!("repo-{}", hex::encode(digest))
}

fn normalize_git_url(url: &str) -> String {
    url.trim()
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_string()
}

fn ensure_origin_matches(repo: &Repository, expected_url: &str) -> Result<()> {
    let origin = repo.find_remote("origin")?;
    let actual = origin.url().ok_or_else(|| {
        QuiverError::Other("cached repository has an origin remote without URL".to_string())
    })?;
    if normalize_git_url(actual) == normalize_git_url(expected_url) {
        return Ok(());
    }
    Err(QuiverError::Other(format!(
        "cached repository origin mismatch: expected '{}', found '{}'. Remove cache directory and retry.",
        expected_url, actual
    )))
}

/// Clone a repository into the cache, or fetch updates if it already exists.
/// Returns the path to the cached repo.
pub fn clone_or_fetch(url: &str) -> Result<PathBuf> {
    let cache = cache_dir()?;
    std::fs::create_dir_all(&cache)?;

    let repo_dir = cache.join(url_to_dirname(url));
    let repo_label = repo_name_from_url(url).unwrap_or_else(|| url.to_string());

    if repo_dir.exists() {
        ui::info(format!(
            "{} cached repository {}",
            ui::keyword("Updating"),
            repo_label
        ));

        let progress = ui::bytes_progress(format!("fetching {repo_label}"));
        let repo = Repository::open(&repo_dir)?;
        ensure_origin_matches(&repo, url)?;
        let mut remote = repo.find_remote("origin")?;
        let callbacks = transfer_progress_callbacks(progress.clone(), repo_label.clone());
        let mut fetch_opts = FetchOptions::new();
        fetch_opts.remote_callbacks(callbacks);
        remote.fetch(&[] as &[&str], Some(&mut fetch_opts), None)?;
        progress.finish_and_clear();
        ui::success(format!(
            "{} repository {}",
            ui::keyword("Updated"),
            repo_label
        ));
        Ok(repo_dir)
    } else {
        ui::info(format!(
            "{} repository {}",
            ui::keyword("Cloning"),
            repo_label
        ));

        let progress = ui::bytes_progress(format!("cloning {repo_label}"));
        let callbacks = transfer_progress_callbacks(progress.clone(), repo_label.clone());
        let mut fetch_opts = FetchOptions::new();
        fetch_opts.remote_callbacks(callbacks);

        RepoBuilder::new()
            .fetch_options(fetch_opts)
            .clone(url, &repo_dir)?;
        progress.finish_and_clear();
        ui::success(format!(
            "{} repository {}",
            ui::keyword("Cloned"),
            repo_label
        ));

        Ok(repo_dir)
    }
}

fn transfer_progress_callbacks(
    progress: ProgressBar,
    repo_label: String,
) -> RemoteCallbacks<'static> {
    let mut callbacks = RemoteCallbacks::new();
    callbacks.transfer_progress(move |stats| {
        update_transfer_progress(&progress, &repo_label, stats);
        true
    });
    callbacks
}

fn update_transfer_progress(progress: &ProgressBar, repo_label: &str, stats: Progress<'_>) {
    let total_objects = stats.total_objects() as u64;
    let received_objects = stats.received_objects() as u64;
    let total_deltas = stats.total_deltas() as u64;
    let indexed_deltas = stats.indexed_deltas() as u64;

    if total_objects > 0 {
        progress.set_length(total_objects);
        progress.set_position(received_objects);
        progress.set_message(format!(
            "{repo_label} objects {received_objects}/{total_objects} deltas {indexed_deltas}/{total_deltas}"
        ));
    } else {
        progress.set_message(format!("{repo_label} preparing remote objects"));
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
    let mut io_error: Option<std::io::Error> = None;
    let mut git_error: Option<git2::Error> = None;
    tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
        if io_error.is_some() || git_error.is_some() {
            return git2::TreeWalkResult::Abort;
        }
        let name = match entry.name() {
            Some(n) => n,
            None => return git2::TreeWalkResult::Ok,
        };
        let path = dest.join(dir).join(name);

        match entry.kind() {
            Some(git2::ObjectType::Tree) => {
                if let Err(err) = std::fs::create_dir_all(&path) {
                    io_error = Some(err);
                    return git2::TreeWalkResult::Abort;
                }
            }
            Some(git2::ObjectType::Blob) => match repo.find_blob(entry.id()) {
                Ok(obj) => {
                    if let Some(parent) = path.parent()
                        && let Err(err) = std::fs::create_dir_all(parent)
                    {
                        io_error = Some(err);
                        return git2::TreeWalkResult::Abort;
                    }
                    if let Err(err) = std::fs::write(&path, obj.content()) {
                        io_error = Some(err);
                        return git2::TreeWalkResult::Abort;
                    }
                }
                Err(err) => {
                    git_error = Some(err);
                    return git2::TreeWalkResult::Abort;
                }
            },
            _ => {}
        }

        git2::TreeWalkResult::Ok
    })?;

    if let Some(err) = io_error {
        return Err(err.into());
    }
    if let Some(err) = git_error {
        return Err(err.into());
    }

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

    let mut semver_tags: Vec<(semver::Version, String)> = tags
        .iter()
        .filter_map(|tag| parse_semver_tag(tag).map(|version| (version, tag.clone())))
        .collect();

    if !semver_tags.is_empty() {
        semver_tags.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        return Ok(semver_tags.last().map(|(_, tag)| tag.clone()));
    }

    // Fall back to lexicographic ordering for non-semver tags.
    tags.sort();
    Ok(tags.last().cloned())
}

fn parse_semver_tag(tag: &str) -> Option<semver::Version> {
    let trimmed = tag.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalized = trimmed
        .strip_prefix('v')
        .or_else(|| trimmed.strip_prefix('V'))
        .unwrap_or(trimmed);
    semver::Version::parse(normalized).ok()
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

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{IndexAddOption, Signature};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_repo_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "quiver_git_test_{}_{}_{}",
            label,
            std::process::id(),
            unique
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn url_cache_key_is_stable_and_not_lossy() {
        let first = url_to_dirname("https://github.com/org/repo.with.dot");
        let second = url_to_dirname("https://github.com/org_repo/with/dot");
        assert_ne!(first, second);
        assert_eq!(
            first,
            url_to_dirname("https://github.com/org/repo.with.dot")
        );
    }

    #[test]
    fn origin_match_allows_normalized_git_urls() {
        let dir = temp_repo_dir("origin_normalized");
        let repo = Repository::init(&dir).unwrap();
        repo.remote("origin", "https://github.com/example/project.git")
            .unwrap();

        assert!(ensure_origin_matches(&repo, "https://github.com/example/project").is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn origin_match_rejects_mismatched_cached_repo() {
        let dir = temp_repo_dir("origin_mismatch");
        let repo = Repository::init(&dir).unwrap();
        repo.remote("origin", "https://github.com/example/project.git")
            .unwrap();

        assert!(ensure_origin_matches(&repo, "https://github.com/example/other").is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    fn init_repo_with_commit(label: &str) -> (PathBuf, Repository) {
        let dir = temp_repo_dir(label);
        let repo = Repository::init(&dir).unwrap();
        std::fs::write(dir.join("README.md"), "test").unwrap();

        let mut index = repo.index().unwrap();
        index
            .add_all(["*"], IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = Signature::now("Quiver Tests", "tests@quiver.local").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
            .unwrap();
        drop(tree);

        (dir, repo)
    }

    fn create_tag(repo: &Repository, name: &str) {
        let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
        repo.tag_lightweight(name, head_commit.as_object(), false)
            .unwrap();
    }

    #[test]
    fn latest_tag_uses_semver_order_instead_of_lexicographic_order() {
        let (dir, repo) = init_repo_with_commit("latest_tag_semver_order");
        create_tag(&repo, "v0.9.0");
        create_tag(&repo, "v0.10.0");
        create_tag(&repo, "v0.11.0");

        let latest = latest_tag(&dir).unwrap();
        assert_eq!(latest.as_deref(), Some("v0.11.0"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn latest_tag_falls_back_for_non_semver_tags() {
        let (dir, repo) = init_repo_with_commit("latest_tag_non_semver");
        create_tag(&repo, "alpha");
        create_tag(&repo, "beta");

        let latest = latest_tag(&dir).unwrap();
        assert_eq!(latest.as_deref(), Some("beta"));

        let _ = std::fs::remove_dir_all(dir);
    }
}
