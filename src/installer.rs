use std::path::Path;

use crate::config::{self, GlobalConfig};
use crate::error::{NuanceError, Result};
use crate::git;
use crate::lockfile::{LockedPackage, LockedPackageKind, Lockfile};
use crate::manifest::Manifest;
use crate::resolver::{self, ResolvedDep, ResolvedScriptDep};

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

/// Run a full local install: resolve → fetch → checksum → place → lock.
pub fn install(project_dir: &Path, frozen: bool) -> Result<()> {
    let manifest = Manifest::from_dir(project_dir)?;
    let lock_path = project_dir.join("mod.lock");
    let modules_dir = project_dir.join(MODULES_DIR);
    let scripts_dir = project_dir.join(SCRIPTS_DIR);

    if manifest.dependencies.is_empty() {
        eprintln!("No dependencies declared in mod.toml.");
        write_activate_overlay(&modules_dir, MODULES_DIR, std::iter::empty::<&str>(), false)?;
        return Ok(());
    }

    // Determine whether to re-resolve or use the lockfile
    let (resolved_modules, resolved_scripts) = if frozen {
        // --frozen: use lockfile only
        if !lock_path.exists() {
            return Err(crate::error::NuanceError::Lockfile(
                "mod.lock not found (required with --frozen)".to_string(),
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
    )
}

/// Run an update: always re-resolve, ignoring existing lockfile.
pub fn update(project_dir: &Path) -> Result<()> {
    let lock_path = project_dir.join("mod.lock");
    // Remove existing lockfile to force re-resolution
    if lock_path.exists() {
        std::fs::remove_file(&lock_path)?;
    }
    install(project_dir, false)
}

/// Run a global install: resolve from `~/.config/nuance/config.toml` and install
/// modules to the global modules directory.
pub fn install_global(frozen: bool) -> Result<()> {
    let config = GlobalConfig::load()?;
    let modules_dir = config.modules_dir()?;
    let lock_path = config::global_lock_path()?;
    let display_dir = modules_dir.display().to_string();

    if config.dependencies.is_empty() {
        eprintln!("No dependencies declared in global config.");
        write_activate_overlay(
            &modules_dir,
            &display_dir,
            std::iter::empty::<&str>(),
            false,
        )?;
        return Ok(());
    }

    let resolved_modules = if frozen {
        if !lock_path.exists() {
            return Err(crate::error::NuanceError::Lockfile(
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
    } else {
        eprintln!("Resolving global dependencies...");
        resolver::resolve_from_deps(&config.dependencies)?
    };

    install_resolved(
        &resolved_modules,
        &[],
        &modules_dir,
        None,
        None,
        &lock_path,
        &display_dir,
    )
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
        modules.iter().map(|dep| dep.name.as_str()),
        scripts_dir.is_some() && !scripts.is_empty(),
    )?;
    if !scripts.is_empty() {
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
        "# Generated by nuance — do not edit\nexport-env {\n    let modules_dir = ($env.FILE_PWD | path join)\n",
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
    let mut activate_script = String::from("# Generated by nuance — do not edit\n");
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
fn install_dep(dep: &ResolvedDep, modules_dir: &Path) -> Result<()> {
    let repo_path = git::clone_or_fetch(&dep.git)?;
    let dest = modules_dir.join(&dep.name);
    git::export_to(&repo_path, &dep.rev, &dest)?;
    Ok(())
}

/// Install a single resolved script dependency into the scripts directory.
fn install_script_dep(dep: &ResolvedScriptDep, scripts_dir: &Path) -> Result<()> {
    let repo_path = git::clone_or_fetch(&dep.git)?;
    let tmp = std::env::temp_dir()
        .join("nuance_script_install")
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

    for pkg in &lockfile.packages {
        if pkg.kind != LockedPackageKind::Module || !config.dependencies.contains_key(&pkg.name) {
            return Ok(true);
        }
    }

    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_temp_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "nuance_installer_test_{}_{}_{}",
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
            project_dir.join("mod.toml"),
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

        let _ = std::fs::remove_dir_all(project_dir);
    }
}
