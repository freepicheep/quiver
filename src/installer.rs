use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::config::{self, GlobalConfig};
use crate::error::Result;
use crate::git;
use crate::lockfile::{LockedPackage, LockedPackageKind, Lockfile};
use crate::manifest::Manifest;
use crate::resolver::{self, ResolvedDep};
use walkdir::WalkDir;

/// The name of the local environment directory.
const NU_ENV_DIR: &str = ".nu-env";
/// The subdirectory within `.nu-env/` where module files are installed.
const MODULES_SUBDIR: &str = "modules";
/// The subdirectory within `.nu-env/` where the nu binary symlink lives.
const BIN_SUBDIR: &str = "bin";

#[derive(Debug, Default)]
struct NupmMetadataHints {
    package_name: Option<String>,
    entry_hint: Option<String>,
}

/// Run a full local install: resolve -> fetch -> checksum -> place -> lock.
pub fn install(project_dir: &Path, frozen: bool) -> Result<()> {
    let manifest = Manifest::from_dir(project_dir)?;
    let lock_path = project_dir.join("quiver.lock");
    let nu_env_dir = project_dir.join(NU_ENV_DIR);
    let modules_dir = nu_env_dir.join(MODULES_SUBDIR);
    let display_name = format!("{NU_ENV_DIR}/{MODULES_SUBDIR}");

    if manifest.dependencies.is_empty() {
        eprintln!("No dependencies declared in nupackage.toml.");
        write_env_nu(&nu_env_dir, &modules_dir)?;
        create_nu_symlink(&nu_env_dir)?;
        write_activate_overlay(&nu_env_dir, project_dir)?;
        return Ok(());
    }

    // Determine whether to re-resolve or use the lockfile
    let resolved_modules = if frozen {
        // --frozen: use lockfile only
        if !lock_path.exists() {
            return Err(crate::error::QuiverError::Lockfile(
                "quiver.lock not found (required with --frozen)".to_string(),
            ));
        }
        let lockfile = Lockfile::from_path(&lock_path)?;
        eprintln!("Using locked dependencies (--frozen).");
        resolver::resolve_from_lock(&lockfile.packages)
    } else if lock_path.exists() {
        let lockfile = Lockfile::from_path(&lock_path)?;

        if !local_lockfile_is_stale(&manifest, &lockfile) {
            eprintln!("Using existing lockfile.");
            resolver::resolve_from_lock(&lockfile.packages)
        } else {
            if !manifest.dependencies.modules.is_empty() {
                eprintln!("Resolving module dependencies...");
            }
            resolver::resolve_from_deps(&manifest.dependencies.modules)?
        }
    } else if manifest.dependencies.modules.is_empty() {
        Vec::new()
    } else {
        // No lockfile yet: resolve from manifest.
        eprintln!("Resolving module dependencies...");
        resolver::resolve_from_deps(&manifest.dependencies.modules)?
    };

    // Install each dependency
    install_resolved(
        &resolved_modules,
        &modules_dir,
        &lock_path,
        &nu_env_dir,
        &display_name,
    )
}

/// Run an update: always re-resolve, ignoring existing lockfile.
pub fn update(project_dir: &Path) -> Result<()> {
    let lock_path = project_dir.join("quiver.lock");
    // Remove existing lockfile to force re-resolution
    if lock_path.exists() {
        std::fs::remove_file(&lock_path)?;
    }
    install(project_dir, false)
}

/// Run a global install: resolve from `~/.config/quiver/config.toml` and install
/// modules to the configured global directory.
pub fn install_global(frozen: bool) -> Result<()> {
    let config = GlobalConfig::load()?;
    let modules_dir = config.modules_dir()?;
    let lock_path = config::global_lock_path()?;
    let display_dir = modules_dir.display().to_string();

    if config.dependencies.is_empty() {
        eprintln!("No dependencies declared in global config.");
        return Ok(());
    }

    let resolved_modules = if frozen {
        if !lock_path.exists() {
            return Err(crate::error::QuiverError::Lockfile(
                "config.lock not found (required with --frozen)".to_string(),
            ));
        }
        let lockfile = Lockfile::from_path(&lock_path)?;
        eprintln!("Using locked global dependencies (--frozen).");
        resolver::resolve_from_lock(&lockfile.packages)
    } else if lock_path.exists() && !is_global_lockfile_stale(&config, &lock_path)? {
        let lockfile = Lockfile::from_path(&lock_path)?;
        eprintln!("Using existing global lockfile.");
        resolver::resolve_from_lock(&lockfile.packages)
    } else if config.dependencies.is_empty() {
        Vec::new()
    } else {
        eprintln!("Resolving global module dependencies...");
        resolver::resolve_from_deps(&config.dependencies)?
    };

    install_resolved_global(&resolved_modules, &modules_dir, &lock_path, &display_dir)
}

/// Install resolved global dependencies and write `config.lock`.
fn install_resolved_global(
    modules: &[ResolvedDep],
    modules_dir: &Path,
    lock_path: &Path,
    modules_display_name: &str,
) -> Result<()> {
    std::fs::create_dir_all(modules_dir)?;

    let mut locked_packages = Vec::new();

    for dep in modules {
        eprintln!(
            "  Installing {}@{}...",
            dep.name,
            &dep.rev[..12.min(dep.rev.len())]
        );
        install_dep(dep, modules_dir)?;

        let dest = modules_dir.join(&dep.name);
        let sha256 = resolver::compute_checksum(&dest)?;

        locked_packages.push(LockedPackage {
            name: dep.name.clone(),
            kind: LockedPackageKind::Module,
            git: dep.git.clone(),
            tag: dep.tag.clone(),
            rev: dep.rev.clone(),
            path: None,
            sha256,
        });
    }

    locked_packages.sort_by(|a, b| a.kind.cmp(&b.kind).then(a.name.cmp(&b.name)));

    let lockfile = Lockfile {
        version: 1,
        packages: locked_packages,
    };
    lockfile.write_to(lock_path)?;

    let module_count = modules.len();
    let module_suffix = if module_count == 1 { "" } else { "s" };

    eprintln!();
    eprintln!("Installed {module_count} module{module_suffix} into {modules_display_name}/");

    Ok(())
}

/// Install a list of resolved dependencies into a target directory and write the lockfile.
fn install_resolved(
    modules: &[ResolvedDep],
    modules_dir: &Path,
    lock_path: &Path,
    nu_env_dir: &Path,
    display_name: &str,
) -> Result<()> {
    std::fs::create_dir_all(modules_dir)?;
    let mut locked_packages = Vec::new();

    for dep in modules {
        eprintln!(
            "  Installing {}@{}...",
            dep.name,
            &dep.rev[..12.min(dep.rev.len())]
        );
        install_dep(dep, modules_dir)?;

        let dest = modules_dir.join(&dep.name);
        let sha256 = resolver::compute_checksum(&dest)?;

        locked_packages.push(LockedPackage {
            name: dep.name.clone(),
            kind: LockedPackageKind::Module,
            git: dep.git.clone(),
            tag: dep.tag.clone(),
            rev: dep.rev.clone(),
            path: None,
            sha256,
        });
    }

    locked_packages.sort_by(|a, b| a.kind.cmp(&b.kind).then(a.name.cmp(&b.name)));

    // Write lockfile
    let lockfile = Lockfile {
        version: 1,
        packages: locked_packages,
    };
    lockfile.write_to(lock_path)?;

    let module_count = modules.len();
    let module_suffix = if module_count == 1 { "" } else { "s" };
    eprintln!();
    eprintln!("Installed {module_count} module{module_suffix} into {display_name}/");

    // Derive project_dir from nu_env_dir (parent of .nu-env)
    let project_dir = nu_env_dir.parent().unwrap_or(nu_env_dir);

    write_env_nu(nu_env_dir, modules_dir)?;
    create_nu_symlink(nu_env_dir)?;
    write_activate_overlay(nu_env_dir, project_dir)?;

    Ok(())
}

/// Generate `.nu-env/activate.nu` with a `nu` alias and deactivate alias.
pub fn write_activate_overlay(nu_env_dir: &Path, project_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(nu_env_dir)?;

    let nu_bin = nu_env_dir.join(BIN_SUBDIR).join("nu");
    let env_nu = nu_env_dir.join("env.nu");
    let nu_bin_literal = nu_string_literal(&nu_bin);
    let env_nu_literal = nu_string_literal(&env_nu);

    let activate_script = format!(
        r#"# Generated by quiver - do not edit
export-env {{
    source-env {env_nu_literal}
}}

# run nu with the env config for this project
export def --wrapped nu [...rest] {{
    ^{nu_bin_literal} --env-config {env_nu_literal} ...$rest
}}

# deactivate the virtual environment for this project
export alias deactivate = overlay hide activate
"#,
    );

    let activate_path = nu_env_dir.join("activate.nu");
    std::fs::write(&activate_path, activate_script)?;
    eprintln!(
        "Generated {}",
        activate_path
            .strip_prefix(project_dir)
            .unwrap_or(&activate_path)
            .display()
    );
    Ok(())
}

/// Generate `.nu-env/env.nu` with `NU_LIB_DIRS` pointing to the modules directory.
pub fn write_env_nu(nu_env_dir: &Path, modules_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(nu_env_dir)?;

    let modules_literal = nu_string_literal(modules_dir);
    let env_script = format!(
        r#"# Generated by quiver - do not edit
export const NU_LIB_DIRS = [
    {modules_literal}
]
"#,
    );

    let env_path = nu_env_dir.join("env.nu");
    std::fs::write(&env_path, env_script)?;
    eprintln!("Generated {}", env_path.display());
    Ok(())
}

/// Create a symlink at `.nu-env/bin/nu` pointing to the user's `nu` binary.
pub fn create_nu_symlink(nu_env_dir: &Path) -> Result<()> {
    let bin_dir = nu_env_dir.join(BIN_SUBDIR);
    std::fs::create_dir_all(&bin_dir)?;

    let symlink_path = bin_dir.join("nu");

    // Remove existing symlink if present
    if symlink_path.exists() || symlink_path.symlink_metadata().is_ok() {
        std::fs::remove_file(&symlink_path)?;
    }

    let nu_path = match detect_nu_path() {
        Some(path) => path,
        None => {
            eprintln!("Warning: could not find 'nu' in PATH; skipping .nu-env/bin/nu symlink.");
            return Ok(());
        }
    };

    #[cfg(unix)]
    std::os::unix::fs::symlink(&nu_path, &symlink_path)?;
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&nu_path, &symlink_path)?;

    eprintln!("Linked .nu-env/bin/nu -> {}", nu_path.display());
    Ok(())
}

/// Detect the absolute path to the user's `nu` binary.
fn detect_nu_path() -> Option<PathBuf> {
    let output = Command::new("which").arg("nu").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let path_str = stdout.trim();
    if path_str.is_empty() {
        None
    } else {
        Some(PathBuf::from(path_str))
    }
}

fn nu_string_literal(path: &Path) -> String {
    format!("{:?}", path.to_string_lossy())
}

/// Install a single resolved dependency into the modules directory.
fn install_dep(dep: &ResolvedDep, modules_dir: &Path) -> Result<String> {
    let repo_path = git::clone_or_fetch(&dep.git)?;
    let dest = modules_dir.join(&dep.name);
    git::export_to(&repo_path, &dep.rev, &dest)?;
    discover_module_use_path(&dest, &dep.name)
}

fn discover_module_use_path(module_root: &Path, dep_name: &str) -> Result<String> {
    let metadata = read_nupm_metadata_hints(module_root)?;
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();

    if let Some(entry_hint) = metadata.entry_hint.as_deref() {
        if let Some(subdir) = module_subpath_from_hint(module_root, entry_hint) {
            push_unique_path(&mut candidates, &mut seen, subdir);
        }
    }

    if let Some(package_name) = metadata.package_name.as_deref() {
        if let Some(subdir) = normalized_relative_path(Path::new(package_name)) {
            if module_root.join(&subdir).join("mod.nu").is_file() {
                push_unique_path(&mut candidates, &mut seen, subdir);
            }
        }
    }

    if module_root.join("mod.nu").is_file() {
        push_unique_path(&mut candidates, &mut seen, PathBuf::new());
    }

    let dep_name_subdir = PathBuf::from(dep_name);
    if module_root.join(&dep_name_subdir).join("mod.nu").is_file() {
        push_unique_path(&mut candidates, &mut seen, dep_name_subdir);
    }

    let mut discovered = find_mod_nu_dirs(module_root);
    discovered.sort_by(|a, b| {
        rank_candidate_dir(a, metadata.package_name.as_deref(), dep_name)
            .cmp(&rank_candidate_dir(
                b,
                metadata.package_name.as_deref(),
                dep_name,
            ))
            .then(path_to_forward_slashes(a).cmp(&path_to_forward_slashes(b)))
    });

    for subdir in discovered {
        push_unique_path(&mut candidates, &mut seen, subdir);
    }

    if let Some(best) = candidates.first() {
        return Ok(module_use_path(dep_name, best));
    }

    eprintln!(
        "Warning: could not locate mod.nu for module '{dep_name}' after install; defaulting to '{dep_name}'."
    );
    Ok(dep_name.to_string())
}

fn read_nupm_metadata_hints(module_root: &Path) -> Result<NupmMetadataHints> {
    let nupm_path = module_root.join("nupm.nuon");
    if !nupm_path.is_file() {
        return Ok(NupmMetadataHints::default());
    }

    let content = std::fs::read_to_string(&nupm_path)?;
    Ok(NupmMetadataHints {
        package_name: extract_nuon_value(&content, &["name", "package_name"]),
        entry_hint: extract_nuon_value(
            &content,
            &[
                "module",
                "module_path",
                "module-dir",
                "entry",
                "entrypoint",
                "main",
            ],
        ),
    })
}

fn extract_nuon_value(content: &str, keys: &[&str]) -> Option<String> {
    for raw_line in content.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }

        for separator in [':', '='] {
            if let Some((lhs, rhs)) = line.split_once(separator) {
                let key = lhs.trim().trim_matches('"').trim_matches('\'');
                if keys
                    .iter()
                    .any(|expected| key.eq_ignore_ascii_case(expected))
                {
                    if let Some(value) = parse_nuon_scalar(rhs) {
                        return Some(value);
                    }
                }
            }
        }
    }

    None
}

fn parse_nuon_scalar(raw_value: &str) -> Option<String> {
    let value = raw_value.trim().trim_end_matches(',').trim();
    if value.is_empty() {
        return None;
    }

    let mut chars = value.chars();
    match chars.next() {
        Some('"') | Some('\'') => {
            let quote = value.chars().next().unwrap_or('"');
            let mut parsed = String::new();
            let mut escaped = false;

            for ch in value[1..].chars() {
                if escaped {
                    parsed.push(ch);
                    escaped = false;
                    continue;
                }

                if ch == '\\' {
                    escaped = true;
                    continue;
                }

                if ch == quote {
                    return if parsed.is_empty() {
                        None
                    } else {
                        Some(parsed)
                    };
                }

                parsed.push(ch);
            }

            None
        }
        _ => {
            let token = value
                .split(|c: char| c.is_whitespace() || matches!(c, ',' | '}' | ']'))
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches('"')
                .trim_matches('\'');

            if token.is_empty() {
                None
            } else {
                Some(token.to_string())
            }
        }
    }
}

fn module_subpath_from_hint(module_root: &Path, hint: &str) -> Option<PathBuf> {
    let normalized_hint = hint.trim().replace('\\', "/");
    if normalized_hint.is_empty() {
        return None;
    }

    let hint_path = normalized_relative_path(Path::new(&normalized_hint))?;
    let subdir = if hint_path.file_name().and_then(|name| name.to_str()) == Some("mod.nu") {
        hint_path
            .parent()
            .map_or_else(PathBuf::new, Path::to_path_buf)
    } else {
        hint_path
    };

    if module_root.join(&subdir).join("mod.nu").is_file() {
        Some(subdir)
    } else {
        None
    }
}

fn normalized_relative_path(path: &Path) -> Option<PathBuf> {
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

fn find_mod_nu_dirs(module_root: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    for entry in WalkDir::new(module_root)
        .follow_links(false)
        .into_iter()
        .filter_map(std::result::Result::ok)
    {
        if !entry.file_type().is_file() || entry.file_name() != "mod.nu" {
            continue;
        }

        if let Ok(relative_file) = entry.path().strip_prefix(module_root) {
            let parent = relative_file.parent().unwrap_or_else(|| Path::new(""));
            if let Some(normalized) = normalized_relative_path(parent) {
                dirs.push(normalized);
            }
        }
    }

    dirs
}

fn rank_candidate_dir(path: &Path, package_name: Option<&str>, dep_name: &str) -> (u8, usize) {
    let basename = path.file_name().and_then(|name| name.to_str());
    let priority = if path.as_os_str().is_empty() {
        0
    } else if package_name.is_some_and(|name| basename == Some(name)) {
        1
    } else if basename == Some(dep_name) {
        2
    } else {
        3
    };

    (priority, path_depth(path))
}

fn path_depth(path: &Path) -> usize {
    path.components()
        .filter(|component| matches!(component, Component::Normal(_)))
        .count()
}

fn push_unique_path(candidates: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>, path: PathBuf) {
    if seen.insert(path.clone()) {
        candidates.push(path);
    }
}

fn module_use_path(dep_name: &str, subdir: &Path) -> String {
    if subdir.as_os_str().is_empty() {
        dep_name.to_string()
    } else {
        format!("{dep_name}/{}", path_to_forward_slashes(subdir))
    }
}

fn path_to_forward_slashes(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Determine whether the module section is stale relative to the local lockfile.
fn local_lockfile_is_stale(manifest: &Manifest, lockfile: &Lockfile) -> bool {
    // Check if all manifest module deps are in the lockfile
    for name in manifest.dependencies.modules.keys() {
        if lockfile
            .find_package(name, LockedPackageKind::Module)
            .is_none()
        {
            return true;
        }
    }

    // Check if lockfile has deps not in manifest or unsupported kinds.
    for pkg in &lockfile.packages {
        match pkg.kind {
            LockedPackageKind::Module => {
                if !manifest.dependencies.modules.contains_key(&pkg.name) {
                    return true;
                }
            }
            LockedPackageKind::Other => return true,
        }
    }

    false
}

/// Check if the global lockfile is stale relative to the global config.
fn is_global_lockfile_stale(config: &GlobalConfig, lock_path: &Path) -> Result<bool> {
    if !lock_path.exists() {
        return Ok(true);
    }

    let lockfile = Lockfile::from_path(lock_path)?;

    for name in config.dependencies.keys() {
        if lockfile
            .find_package(name, LockedPackageKind::Module)
            .is_none()
        {
            return Ok(true);
        }
    }

    for pkg in &lockfile.packages {
        match pkg.kind {
            LockedPackageKind::Module => {
                if !config.dependencies.contains_key(&pkg.name) {
                    return Ok(true);
                }
            }
            LockedPackageKind::Other => return Ok(true),
        }
    }

    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_temp_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "quiver_installer_test_{}_{}_{}",
            label,
            std::process::id(),
            unique
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writes_activate_overlay() {
        let project_dir = make_temp_dir("activate_overlay");
        let nu_env_dir = project_dir.join(".nu-env");

        write_activate_overlay(&nu_env_dir, &project_dir).unwrap();

        let activate = std::fs::read_to_string(nu_env_dir.join("activate.nu")).unwrap();
        assert!(activate.contains("export-env {"));
        assert!(activate.contains("source-env"));
        assert!(activate.contains("export def --wrapped nu [...rest]"));
        assert!(activate.contains("^"));
        assert!(activate.contains(".nu-env/bin/nu"));
        assert!(activate.contains("--env-config"));
        assert!(activate.contains(".nu-env/env.nu"));
        assert!(activate.contains("export alias deactivate = overlay hide activate"));

        let _ = std::fs::remove_dir_all(project_dir);
    }

    #[test]
    fn writes_env_nu() {
        let nu_env_dir = make_temp_dir("env_nu");
        let modules_dir = nu_env_dir.join("modules");
        std::fs::create_dir_all(&modules_dir).unwrap();

        write_env_nu(&nu_env_dir, &modules_dir).unwrap();

        let env_nu = std::fs::read_to_string(nu_env_dir.join("env.nu")).unwrap();
        assert!(env_nu.contains("export const NU_LIB_DIRS"));
        assert!(env_nu.contains(&modules_dir.display().to_string()));

        let _ = std::fs::remove_dir_all(nu_env_dir);
    }

    #[test]
    fn discovers_nested_module_path_from_nupm_metadata() {
        let module_root = make_temp_dir("nupm_nested");
        let nested_dir = module_root.join("nu-salesforce");
        std::fs::create_dir_all(&nested_dir).unwrap();
        std::fs::write(nested_dir.join("mod.nu"), "# nested module").unwrap();
        std::fs::write(module_root.join("nupm.nuon"), "{ name: \"nu-salesforce\" }").unwrap();

        let use_path = discover_module_use_path(&module_root, "nu-salesforce").unwrap();
        assert_eq!(use_path, "nu-salesforce/nu-salesforce");

        let _ = std::fs::remove_dir_all(module_root);
    }

    #[test]
    fn discovers_root_module_path_without_nupm_metadata() {
        let module_root = make_temp_dir("root_module");
        std::fs::write(module_root.join("mod.nu"), "# root module").unwrap();

        let use_path = discover_module_use_path(&module_root, "nu-foo").unwrap();
        assert_eq!(use_path, "nu-foo");

        let _ = std::fs::remove_dir_all(module_root);
    }

    #[test]
    fn discovers_nested_module_path_without_nupm_metadata() {
        let module_root = make_temp_dir("nested_module_without_nupm");
        let nested_dir = module_root.join("nu-tools");
        std::fs::create_dir_all(&nested_dir).unwrap();
        std::fs::write(nested_dir.join("mod.nu"), "# nested module").unwrap();

        let use_path = discover_module_use_path(&module_root, "nu-tools").unwrap();
        assert_eq!(use_path, "nu-tools/nu-tools");

        let _ = std::fs::remove_dir_all(module_root);
    }

    #[test]
    fn local_lockfile_staleness_detects_module_mismatches() {
        let manifest = Manifest::from_str(
            r#"[package]
name = "demo"
version = "0.1.0"

[dependencies.modules]
nu-salesforce = { git = "https://github.com/freepicheep/nu-salesforce", tag = "v0.3.0" }
"#,
        )
        .unwrap();
        let lockfile = Lockfile::from_str(
            r#"version = 1

[[package]]
name = "other-module"
git = "https://github.com/example/other"
tag = "v1.0.0"
rev = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
sha256 = "aaa"
"#,
        )
        .unwrap();

        assert!(local_lockfile_is_stale(&manifest, &lockfile));
    }

    #[test]
    fn local_lockfile_staleness_detects_unknown_artifacts() {
        let manifest = Manifest::from_str(
            r#"[package]
name = "demo"
version = "0.1.0"

[dependencies.modules]
nu-salesforce = { git = "https://github.com/freepicheep/nu-salesforce", tag = "v0.3.0" }
"#,
        )
        .unwrap();
        let lockfile = Lockfile::from_str(
            r#"version = 1

[[package]]
name = "nu-salesforce"
git = "https://github.com/freepicheep/nu-salesforce"
tag = "v0.3.0"
rev = "cccccccccccccccccccccccccccccccccccccccc"
sha256 = "ccc"

[[package]]
name = "future"
kind = "plugin"
git = "https://github.com/example/future"
tag = "v1.0.0"
rev = "dddddddddddddddddddddddddddddddddddddddd"
sha256 = "ddd"
"#,
        )
        .unwrap();

        assert!(local_lockfile_is_stale(&manifest, &lockfile));
    }

    #[test]
    fn frozen_install_without_dependencies_writes_activate_overlay() {
        let project_dir = make_temp_dir("empty_manifest");
        std::fs::write(
            project_dir.join("nupackage.toml"),
            r#"[package]
name = "demo"
version = "0.1.0"
"#,
        )
        .unwrap();

        install(&project_dir, true).unwrap();

        let nu_env = project_dir.join(".nu-env");
        let activate = std::fs::read_to_string(nu_env.join("activate.nu")).unwrap();
        assert!(activate.contains("export def --wrapped nu [...rest]"));
        assert!(activate.contains("source-env"));
        assert!(activate.contains("export alias deactivate = overlay hide activate"));

        let env_nu = std::fs::read_to_string(nu_env.join("env.nu")).unwrap();
        assert!(env_nu.contains("export const NU_LIB_DIRS"));
        assert!(env_nu.contains(".nu-env/modules"));

        let _ = std::fs::remove_dir_all(project_dir);
    }

    #[test]
    fn global_lockfile_staleness_detects_missing_module_entry() {
        let root = make_temp_dir("global_lock_missing_module");
        let lock_path = root.join("config.lock");
        std::fs::write(
            &lock_path,
            r#"version = 1

[[package]]
name = "other"
git = "https://github.com/example/other"
tag = "v1.0.0"
rev = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
sha256 = "aaa"
"#,
        )
        .unwrap();

        let config = GlobalConfig {
            modules_dir: None,
            default_git_provider: "github".to_string(),
            dependencies: HashMap::from([(
                "nu-utils".to_string(),
                crate::manifest::DependencySpec {
                    git: "https://github.com/example/nu-utils".to_string(),
                    tag: Some("v1.0.0".to_string()),
                    rev: None,
                    branch: None,
                },
            )]),
        };

        assert!(is_global_lockfile_stale(&config, &lock_path).unwrap());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn global_lockfile_staleness_accepts_matching_module_entries() {
        let root = make_temp_dir("global_lock_fresh");
        let lock_path = root.join("config.lock");
        std::fs::write(
            &lock_path,
            r#"version = 1

[[package]]
name = "nu-utils"
git = "https://github.com/example/nu-utils"
tag = "v1.0.0"
rev = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
sha256 = "aaa"
"#,
        )
        .unwrap();

        let config = GlobalConfig {
            modules_dir: None,
            default_git_provider: "github".to_string(),
            dependencies: HashMap::from([(
                "nu-utils".to_string(),
                crate::manifest::DependencySpec {
                    git: "https://github.com/example/nu-utils".to_string(),
                    tag: Some("v1.0.0".to_string()),
                    rev: None,
                    branch: None,
                },
            )]),
        };

        assert!(!is_global_lockfile_stale(&config, &lock_path).unwrap());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn create_nu_symlink_creates_symlink() {
        let nu_env_dir = make_temp_dir("symlink_test");

        create_nu_symlink(&nu_env_dir).unwrap();

        let symlink_path = nu_env_dir.join("bin").join("nu");
        // If nu is available in PATH, symlink should exist
        if symlink_path.exists() {
            assert!(
                symlink_path
                    .symlink_metadata()
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            let target = std::fs::read_link(&symlink_path).unwrap();
            assert!(target.to_string_lossy().contains("nu"));
        }
        // If nu is not in PATH, the function gracefully skips

        let _ = std::fs::remove_dir_all(nu_env_dir);
    }

    #[test]
    fn create_nu_symlink_replaces_existing() {
        let nu_env_dir = make_temp_dir("symlink_replace");
        let bin_dir = nu_env_dir.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        // Create a dummy file where the symlink would go
        std::fs::write(bin_dir.join("nu"), "dummy").unwrap();

        create_nu_symlink(&nu_env_dir).unwrap();

        let symlink_path = bin_dir.join("nu");
        if symlink_path.exists() {
            // Should have replaced the dummy file with a proper symlink
            assert!(
                symlink_path
                    .symlink_metadata()
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }

        let _ = std::fs::remove_dir_all(nu_env_dir);
    }

    #[test]
    fn nu_string_literal_escapes_paths() {
        let literal = nu_string_literal(Path::new(r#"/tmp/dir "with quote"/nu"#));
        assert_eq!(literal, r#""/tmp/dir \"with quote\"/nu""#);
    }
}
