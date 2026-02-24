use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use crate::config::{self, GlobalConfig};
use crate::error::{NuanceError, Result};
use crate::git;
use crate::lockfile::{LockedPackage, LockedPackageKind, Lockfile};
use crate::manifest::{Manifest, ScriptDependencySpec};
use crate::resolver::{self, ResolvedDep, ResolvedScriptDep};
use walkdir::WalkDir;

/// The name of the directory where local dependencies are installed.
const MODULES_DIR: &str = ".nu_modules";
/// The name of the directory where local script dependencies are installed.
const SCRIPTS_DIR: &str = ".nu_scripts";

#[derive(Debug, Clone, Copy, Default)]
struct LocalLockfileStaleness {
    modules: bool,
    scripts: bool,
}

impl LocalLockfileStaleness {
    fn is_stale(self) -> bool {
        self.modules || self.scripts
    }
}

#[derive(Debug, Default)]
struct NupmMetadataHints {
    package_name: Option<String>,
    entry_hint: Option<String>,
}

/// Run a full local install: resolve → fetch → checksum → place → lock.
pub fn install(project_dir: &Path, frozen: bool) -> Result<()> {
    let manifest = Manifest::from_dir(project_dir)?;
    let lock_path = project_dir.join("quiver.lock");
    let modules_dir = project_dir.join(MODULES_DIR);
    let scripts_dir = project_dir.join(SCRIPTS_DIR);

    if manifest.dependencies.is_empty() {
        eprintln!("No dependencies declared in nupackage.toml.");
        write_activate_overlay(&modules_dir, MODULES_DIR, std::iter::empty::<&str>(), false)?;
        write_script_activate(&scripts_dir, SCRIPTS_DIR, std::iter::empty::<&str>())?;
        return Ok(());
    }

    // Determine whether to re-resolve or use the lockfile
    let (resolved_modules, resolved_scripts) = if frozen {
        // --frozen: use lockfile only
        if !lock_path.exists() {
            return Err(crate::error::NuanceError::Lockfile(
                "quiver.lock not found (required with --frozen)".to_string(),
            ));
        }
        let lockfile = Lockfile::from_path(&lock_path)?;
        eprintln!("Using locked dependencies (--frozen).");
        (
            resolver::resolve_from_lock(&lockfile.packages),
            resolver::resolve_scripts_from_lock(&lockfile.packages),
        )
    } else if lock_path.exists() {
        let lockfile = Lockfile::from_path(&lock_path)?;
        let staleness = local_lockfile_staleness(&manifest, &lockfile);

        if !staleness.is_stale() {
            eprintln!("Using existing lockfile.");
            (
                resolver::resolve_from_lock(&lockfile.packages),
                resolver::resolve_scripts_from_lock(&lockfile.packages),
            )
        } else {
            let modules = if staleness.modules {
                if !manifest.dependencies.modules.is_empty() {
                    eprintln!("Resolving module dependencies...");
                }
                resolver::resolve_from_deps(&manifest.dependencies.modules)?
            } else {
                resolver::resolve_from_lock(&lockfile.packages)
            };

            let scripts = if staleness.scripts {
                if !manifest.dependencies.scripts.is_empty() {
                    eprintln!("Resolving script dependencies...");
                }
                resolver::resolve_scripts_from_deps(&manifest.dependencies.scripts)?
            } else {
                resolver::resolve_scripts_from_lock(&lockfile.packages)
            };

            (modules, scripts)
        }
    } else {
        // No lockfile yet: resolve from manifest.
        let modules = if manifest.dependencies.modules.is_empty() {
            Vec::new()
        } else {
            eprintln!("Resolving module dependencies...");
            resolver::resolve_from_deps(&manifest.dependencies.modules)?
        };
        let scripts = if manifest.dependencies.scripts.is_empty() {
            Vec::new()
        } else {
            eprintln!("Resolving script dependencies...");
            resolver::resolve_scripts_from_deps(&manifest.dependencies.scripts)?
        };
        (modules, scripts)
    };

    // Install each dependency
    install_resolved(
        &resolved_modules,
        &resolved_scripts,
        &modules_dir,
        Some(&scripts_dir),
        Some(SCRIPTS_DIR),
        &lock_path,
        MODULES_DIR,
        !resolved_scripts.is_empty(),
        true,
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
/// modules/scripts to the configured global directories.
pub fn install_global(frozen: bool) -> Result<()> {
    let config = GlobalConfig::load()?;
    let modules_dir = config.modules_dir()?;
    let scripts_dir = config.scripts_dir()?;
    let scripts_autoload_dir = config.scripts_autoload_dir()?;
    let lock_path = config::global_lock_path()?;
    let display_dir = modules_dir.display().to_string();
    let scripts_display_dir = scripts_dir.display().to_string();
    let scripts_autoload_display_dir = scripts_autoload_dir.display().to_string();

    if config.dependencies.is_empty() && config.scripts.is_empty() {
        eprintln!("No dependencies declared in global config.");
        write_activate_overlay(
            &modules_dir,
            &display_dir,
            std::iter::empty::<&str>(),
            false,
        )?;
        return Ok(());
    }

    let (resolved_modules, resolved_scripts) = if frozen {
        if !lock_path.exists() {
            return Err(crate::error::NuanceError::Lockfile(
                "config.lock not found (required with --frozen)".to_string(),
            ));
        }
        let lockfile = Lockfile::from_path(&lock_path)?;
        eprintln!("Using locked global dependencies (--frozen).");
        (
            resolver::resolve_from_lock(&lockfile.packages),
            resolver::resolve_scripts_from_lock(&lockfile.packages),
        )
    } else if lock_path.exists() && !is_global_lockfile_stale(&config, &lock_path)? {
        let lockfile = Lockfile::from_path(&lock_path)?;
        eprintln!("Using existing global lockfile.");
        (
            resolver::resolve_from_lock(&lockfile.packages),
            resolver::resolve_scripts_from_lock(&lockfile.packages),
        )
    } else {
        let modules = if config.dependencies.is_empty() {
            Vec::new()
        } else {
            eprintln!("Resolving global module dependencies...");
            resolver::resolve_from_deps(&config.dependencies)?
        };
        let scripts = if config.scripts.is_empty() {
            Vec::new()
        } else {
            eprintln!("Resolving global script dependencies...");
            resolver::resolve_scripts_from_deps(&global_script_specs(&config))?
        };
        (modules, scripts)
    };

    install_resolved_global(
        &config,
        &resolved_modules,
        &resolved_scripts,
        &modules_dir,
        &scripts_dir,
        &scripts_autoload_dir,
        &lock_path,
        &display_dir,
        &scripts_display_dir,
        &scripts_autoload_display_dir,
    )
}

fn global_script_specs(config: &GlobalConfig) -> HashMap<String, ScriptDependencySpec> {
    config
        .scripts
        .iter()
        .map(|(name, spec)| (name.clone(), spec.to_script_dependency_spec()))
        .collect()
}

/// Install resolved global dependencies and write `config.lock`.
fn install_resolved_global(
    config: &GlobalConfig,
    modules: &[ResolvedDep],
    scripts: &[ResolvedScriptDep],
    modules_dir: &Path,
    scripts_dir: &Path,
    scripts_autoload_dir: &Path,
    lock_path: &Path,
    modules_display_name: &str,
    scripts_display_name: &str,
    scripts_autoload_display_name: &str,
) -> Result<()> {
    std::fs::create_dir_all(modules_dir)?;
    std::fs::create_dir_all(scripts_dir)?;
    std::fs::create_dir_all(scripts_autoload_dir)?;

    let mut locked_packages = Vec::new();
    let mut module_use_paths = Vec::new();
    let mut regular_script_count = 0usize;
    let mut autoload_script_count = 0usize;

    for dep in modules {
        eprintln!(
            "  Installing {}@{}...",
            dep.name,
            &dep.rev[..12.min(dep.rev.len())]
        );
        let module_use_path = install_dep(dep, modules_dir)?;
        module_use_paths.push(module_use_path);

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

    for dep in scripts {
        eprintln!(
            "  Installing script {}@{}...",
            dep.name,
            &dep.rev[..12.min(dep.rev.len())]
        );

        let install_to_autoload = config.script_is_autoload(&dep.name);
        let (target_dir, stale_dir) = if install_to_autoload {
            autoload_script_count += 1;
            (scripts_autoload_dir, scripts_dir)
        } else {
            regular_script_count += 1;
            (scripts_dir, scripts_autoload_dir)
        };

        let stale_path = stale_dir.join(format!("{}.nu", dep.name));
        if stale_path.exists() {
            std::fs::remove_file(&stale_path)?;
        }

        install_script_dep(dep, target_dir)?;

        let dest = target_dir.join(format!("{}.nu", dep.name));
        let sha256 = resolver::compute_checksum_file(&dest)?;

        locked_packages.push(LockedPackage {
            name: dep.name.clone(),
            kind: LockedPackageKind::Script,
            git: dep.git.clone(),
            tag: dep.tag.clone(),
            rev: dep.rev.clone(),
            path: Some(dep.path.clone()),
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
    let scripts_suffix = if regular_script_count == 1 { "" } else { "s" };
    let autoload_suffix = if autoload_script_count == 1 { "" } else { "s" };

    eprintln!();
    if module_count > 0 {
        eprintln!("Installed {module_count} module{module_suffix} into {modules_display_name}/");
    }
    if regular_script_count > 0 {
        eprintln!(
            "Installed {regular_script_count} script{scripts_suffix} into {scripts_display_name}/"
        );
    }
    if autoload_script_count > 0 {
        eprintln!(
            "Installed {autoload_script_count} autoload script{autoload_suffix} into {scripts_autoload_display_name}/"
        );
    }

    write_activate_overlay(
        modules_dir,
        modules_display_name,
        module_use_paths.iter().map(|path| path.as_str()),
        false,
    )?;
    Ok(())
}

/// Install a list of resolved dependencies into a target directory and write the lockfile.
fn install_resolved(
    modules: &[ResolvedDep],
    scripts: &[ResolvedScriptDep],
    modules_dir: &Path,
    scripts_dir: Option<&Path>,
    scripts_display_name: Option<&str>,
    lock_path: &Path,
    display_name: &str,
    include_scripts_in_overlay: bool,
    write_script_activate_file: bool,
) -> Result<()> {
    std::fs::create_dir_all(modules_dir)?;
    let mut locked_packages = Vec::new();
    let mut module_use_paths = Vec::new();

    for dep in modules {
        eprintln!(
            "  Installing {}@{}...",
            dep.name,
            &dep.rev[..12.min(dep.rev.len())]
        );
        let module_use_path = install_dep(dep, modules_dir)?;
        module_use_paths.push(module_use_path);

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

    if let Some(scripts_dir) = scripts_dir {
        std::fs::create_dir_all(scripts_dir)?;
        for dep in scripts {
            eprintln!(
                "  Installing script {}@{}...",
                dep.name,
                &dep.rev[..12.min(dep.rev.len())]
            );
            install_script_dep(dep, scripts_dir)?;

            let dest = scripts_dir.join(format!("{}.nu", dep.name));
            let sha256 = resolver::compute_checksum_file(&dest)?;

            locked_packages.push(LockedPackage {
                name: dep.name.clone(),
                kind: LockedPackageKind::Script,
                git: dep.git.clone(),
                tag: dep.tag.clone(),
                rev: dep.rev.clone(),
                path: Some(dep.path.clone()),
                sha256,
            });
        }
    } else if !scripts.is_empty() {
        return Err(NuanceError::Other(
            "internal error: script dependencies provided without a scripts directory".to_string(),
        ));
    }

    locked_packages.sort_by(|a, b| a.kind.cmp(&b.kind).then(a.name.cmp(&b.name)));

    // Write lockfile
    let lockfile = Lockfile {
        version: 1,
        packages: locked_packages,
    };
    lockfile.write_to(lock_path)?;

    let module_count = modules.len();
    let script_count = scripts.len();
    let module_suffix = if module_count == 1 { "" } else { "s" };
    let script_suffix = if script_count == 1 { "" } else { "s" };
    eprintln!();
    match (module_count, script_count, scripts_display_name) {
        (m, 0, _) => eprintln!("Installed {m} module{module_suffix} into {display_name}/"),
        (0, s, Some(scripts_name)) => {
            eprintln!("Installed {s} script{script_suffix} into {scripts_name}/")
        }
        (m, s, Some(scripts_name)) => eprintln!(
            "Installed {m} module{module_suffix} into {display_name}/ and {s} script{script_suffix} into {scripts_name}/"
        ),
        (m, s, None) => {
            let total = m + s;
            let artifact_suffix = if total == 1 { "" } else { "s" };
            eprintln!("Installed {total} artifact{artifact_suffix} into {display_name}/");
        }
    }

    write_activate_overlay(
        modules_dir,
        display_name,
        module_use_paths.iter().map(|path| path.as_str()),
        include_scripts_in_overlay,
    )?;
    if write_script_activate_file {
        if let (Some(scripts_dir), Some(scripts_display_name)) = (scripts_dir, scripts_display_name)
        {
            write_script_activate(
                scripts_dir,
                scripts_display_name,
                scripts.iter().map(|dep| dep.name.as_str()),
            )?;
        }
    }

    Ok(())
}

fn write_activate_overlay<IM, SM>(
    modules_dir: &Path,
    display_name: &str,
    module_names: IM,
    include_scripts_dir: bool,
) -> Result<()>
where
    IM: IntoIterator<Item = SM>,
    SM: AsRef<str>,
{
    std::fs::create_dir_all(modules_dir)?;

    let activate_path = modules_dir.join("activate.nu");
    let mut activate_script = String::from(
        "# Generated by quiver — do not edit\nexport-env {\n    let modules_dir = ($env.FILE_PWD | path join)\n",
    );

    if include_scripts_dir {
        activate_script.push_str(
            "    let scripts_dir = ($modules_dir | path dirname | path join \".nu_scripts\")\n",
        );
        activate_script.push_str(
            "    $env.NU_LIB_DIRS = ($env.NU_LIB_DIRS | default [] | append $modules_dir | append $scripts_dir)\n",
        );
    } else {
        activate_script.push_str(
            "    $env.NU_LIB_DIRS = ($env.NU_LIB_DIRS | default [] | append $modules_dir)\n",
        );
    }

    activate_script.push_str("}\n\n");

    for module_name in module_names {
        activate_script.push_str("export use ");
        activate_script.push_str(module_name.as_ref());
        activate_script.push_str(" *\n");
    }
    activate_script.push_str("\nexport alias deactivate = overlay hide activate\n");

    std::fs::write(&activate_path, activate_script)?;
    eprintln!("Generated {}/activate.nu", display_name);
    Ok(())
}

fn write_script_activate<IS, SS>(
    scripts_dir: &Path,
    display_name: &str,
    script_names: IS,
) -> Result<()>
where
    IS: IntoIterator<Item = SS>,
    SS: AsRef<str>,
{
    std::fs::create_dir_all(scripts_dir)?;

    let activate_path = scripts_dir.join("activate.nu");
    let mut activate_script = String::from("# Generated by quiver — do not edit\n");
    for script_name in script_names {
        activate_script.push_str("source ");
        activate_script.push_str(script_name.as_ref());
        activate_script.push_str(".nu\n");
    }

    std::fs::write(&activate_path, activate_script)?;
    eprintln!("Generated {}/activate.nu", display_name);
    Ok(())
}

/// Install a single resolved dependency into the modules directory.
fn install_dep(dep: &ResolvedDep, modules_dir: &Path) -> Result<String> {
    let repo_path = git::clone_or_fetch(&dep.git)?;
    let dest = modules_dir.join(&dep.name);
    git::export_to(&repo_path, &dep.rev, &dest)?;
    discover_module_use_path(&dest, &dep.name)
}

/// Install a single resolved script dependency into the scripts directory.
fn install_script_dep(dep: &ResolvedScriptDep, scripts_dir: &Path) -> Result<()> {
    let repo_path = git::clone_or_fetch(&dep.git)?;
    let tmp = std::env::temp_dir()
        .join("quiver_script_install")
        .join(format!("{}_{}_{}", dep.name, std::process::id(), dep.rev));

    git::export_to(&repo_path, &dep.rev, &tmp)?;

    let src = tmp.join(&dep.path);
    if !src.is_file() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(NuanceError::Manifest(format!(
            "script dependency '{}': path '{}' was not found in the resolved repository revision",
            dep.name, dep.path
        )));
    }

    std::fs::create_dir_all(scripts_dir)?;
    let dest = scripts_dir.join(format!("{}.nu", dep.name));
    std::fs::copy(&src, &dest)?;
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(())
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

/// Determine whether module/script sections are stale relative to the local lockfile.
fn local_lockfile_staleness(manifest: &Manifest, lockfile: &Lockfile) -> LocalLockfileStaleness {
    let mut staleness = LocalLockfileStaleness::default();

    // Check if all manifest module deps are in the lockfile
    for name in manifest.dependencies.modules.keys() {
        if lockfile
            .find_package(name, LockedPackageKind::Module)
            .is_none()
        {
            staleness.modules = true;
            break;
        }
    }

    // Check if all manifest script deps are in the lockfile
    for name in manifest.dependencies.scripts.keys() {
        if lockfile
            .find_package(name, LockedPackageKind::Script)
            .is_none()
        {
            staleness.scripts = true;
            break;
        }
    }

    // Check if lockfile has deps not in manifest, grouped by kind
    for pkg in &lockfile.packages {
        match pkg.kind {
            LockedPackageKind::Module => {
                if !manifest.dependencies.modules.contains_key(&pkg.name) {
                    staleness.modules = true;
                }
            }
            LockedPackageKind::Script => {
                if !manifest.dependencies.scripts.contains_key(&pkg.name) || pkg.path.is_none() {
                    staleness.scripts = true;
                }
            }
        }

        if staleness.is_stale() {
            break;
        }
    }

    staleness
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

    for name in config.scripts.keys() {
        if lockfile
            .find_package(name, LockedPackageKind::Script)
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
            LockedPackageKind::Script => {
                if !config.scripts.contains_key(&pkg.name) || pkg.path.is_none() {
                    return Ok(true);
                }
            }
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
    fn writes_activate_overlay_with_modules() {
        let modules_dir = make_temp_dir("with_modules");

        write_activate_overlay(&modules_dir, ".nu_modules", ["nu-foo", "nu-bar"], false).unwrap();

        let activate = std::fs::read_to_string(modules_dir.join("activate.nu")).unwrap();
        assert!(activate.contains("export use nu-foo *"));
        assert!(activate.contains("export use nu-bar *"));
        assert!(activate.contains("export alias deactivate = overlay hide activate"));

        let _ = std::fs::remove_dir_all(modules_dir);
    }

    #[test]
    fn writes_activate_overlay_without_modules() {
        let modules_dir = make_temp_dir("without_modules");

        write_activate_overlay(
            &modules_dir,
            ".nu_modules",
            std::iter::empty::<&str>(),
            false,
        )
        .unwrap();

        let activate = std::fs::read_to_string(modules_dir.join("activate.nu")).unwrap();
        assert!(!activate.lines().any(|line| line.starts_with("export use ")));
        assert!(activate.contains("export alias deactivate = overlay hide activate"));

        let _ = std::fs::remove_dir_all(modules_dir);
    }

    #[test]
    fn writes_activate_overlay_with_scripts_dir() {
        let modules_dir = make_temp_dir("with_scripts");

        write_activate_overlay(&modules_dir, ".nu_modules", ["nu-foo"], true).unwrap();

        let activate = std::fs::read_to_string(modules_dir.join("activate.nu")).unwrap();
        assert!(activate.contains(
            "let scripts_dir = ($modules_dir | path dirname | path join \".nu_scripts\")"
        ));
        assert!(activate.contains("append $scripts_dir"));
        assert!(!activate.lines().any(|line| line.starts_with("source ")));
        let _ = std::fs::remove_dir_all(modules_dir);
    }

    #[test]
    fn writes_script_activate_with_scripts() {
        let scripts_dir = make_temp_dir("scripts_activate");

        write_script_activate(&scripts_dir, ".nu_scripts", ["quickfix", "zeta"]).unwrap();

        let activate = std::fs::read_to_string(scripts_dir.join("activate.nu")).unwrap();
        assert!(activate.contains("source quickfix.nu"));
        assert!(activate.contains("source zeta.nu"));
        let _ = std::fs::remove_dir_all(scripts_dir);
    }

    #[test]
    fn writes_script_activate_without_scripts() {
        let scripts_dir = make_temp_dir("scripts_activate_empty");

        write_script_activate(&scripts_dir, ".nu_scripts", std::iter::empty::<&str>()).unwrap();

        let activate = std::fs::read_to_string(scripts_dir.join("activate.nu")).unwrap();
        assert!(!activate.lines().any(|line| line.starts_with("source ")));
        let _ = std::fs::remove_dir_all(scripts_dir);
    }

    #[test]
    fn install_resolved_writes_empty_script_activate_when_no_scripts() {
        let root = make_temp_dir("install_resolved_empty_scripts");
        let modules_dir = root.join(".nu_modules");
        let scripts_dir = root.join(".nu_scripts");
        let lock_path = root.join("quiver.lock");

        install_resolved(
            &[],
            &[],
            &modules_dir,
            Some(&scripts_dir),
            Some(".nu_scripts"),
            &lock_path,
            ".nu_modules",
            false,
            true,
        )
        .unwrap();

        let activate = std::fs::read_to_string(scripts_dir.join("activate.nu")).unwrap();
        assert!(!activate.lines().any(|line| line.starts_with("source ")));
        let _ = std::fs::remove_dir_all(root);
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
    fn local_lockfile_staleness_tracks_modules_and_scripts_independently() {
        let manifest = Manifest::from_str(
            r#"[package]
name = "demo"
version = "0.1.0"

[dependencies.modules]
nu-salesforce = { git = "https://github.com/freepicheep/nu-salesforce", tag = "v0.3.0" }

[dependencies.scripts]
twitter = { git = "https://github.com/nushell/nu_scripts", path = "sourced/webscraping/twitter.nu", tag = "v1.0.0" }
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

[[package]]
name = "twitter"
kind = "script"
git = "https://github.com/nushell/nu_scripts"
path = "sourced/webscraping/twitter.nu"
tag = "v1.0.0"
rev = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
sha256 = "bbb"
"#,
        )
        .unwrap();

        let staleness = local_lockfile_staleness(&manifest, &lockfile);
        assert!(staleness.modules);
        assert!(!staleness.scripts);
    }

    #[test]
    fn local_lockfile_staleness_detects_invalid_script_entries() {
        let manifest = Manifest::from_str(
            r#"[package]
name = "demo"
version = "0.1.0"

[dependencies.modules]
nu-salesforce = { git = "https://github.com/freepicheep/nu-salesforce", tag = "v0.3.0" }

[dependencies.scripts]
twitter = { git = "https://github.com/nushell/nu_scripts", path = "sourced/webscraping/twitter.nu", tag = "v1.0.0" }
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
name = "twitter"
kind = "script"
git = "https://github.com/nushell/nu_scripts"
tag = "v1.0.0"
rev = "dddddddddddddddddddddddddddddddddddddddd"
sha256 = "ddd"
"#,
        )
        .unwrap();

        let staleness = local_lockfile_staleness(&manifest, &lockfile);
        assert!(!staleness.modules);
        assert!(staleness.scripts);
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

        let activate =
            std::fs::read_to_string(project_dir.join(".nu_modules").join("activate.nu")).unwrap();
        assert!(!activate.lines().any(|line| line.starts_with("export use ")));
        assert!(activate.contains("export alias deactivate = overlay hide activate"));

        let script_activate =
            std::fs::read_to_string(project_dir.join(".nu_scripts").join("activate.nu")).unwrap();
        assert!(
            !script_activate
                .lines()
                .any(|line| line.starts_with("source "))
        );

        let _ = std::fs::remove_dir_all(project_dir);
    }

    #[test]
    fn global_lockfile_staleness_detects_missing_script_entry() {
        let root = make_temp_dir("global_lock_missing_script");
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
            scripts_dir: None,
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
            scripts: HashMap::from([(
                "quickfix".to_string(),
                crate::config::GlobalScriptDependencySpec {
                    git: "https://github.com/example/nu-scripts".to_string(),
                    path: "scripts/quickfix.nu".to_string(),
                    tag: Some("v1.0.0".to_string()),
                    rev: None,
                    branch: None,
                    autoload: false,
                },
            )]),
        };

        assert!(is_global_lockfile_stale(&config, &lock_path).unwrap());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn global_lockfile_staleness_accepts_matching_module_and_script_entries() {
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

[[package]]
name = "quickfix"
kind = "script"
git = "https://github.com/example/nu-scripts"
path = "scripts/quickfix.nu"
tag = "v1.0.0"
rev = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
sha256 = "bbb"
"#,
        )
        .unwrap();

        let config = GlobalConfig {
            modules_dir: None,
            scripts_dir: None,
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
            scripts: HashMap::from([(
                "quickfix".to_string(),
                crate::config::GlobalScriptDependencySpec {
                    git: "https://github.com/example/nu-scripts".to_string(),
                    path: "scripts/quickfix.nu".to_string(),
                    tag: Some("v1.0.0".to_string()),
                    rev: None,
                    branch: None,
                    autoload: true,
                },
            )]),
        };

        assert!(!is_global_lockfile_stale(&config, &lock_path).unwrap());
        let _ = std::fs::remove_dir_all(root);
    }
}
