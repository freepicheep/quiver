use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use semver::{Version, VersionReq};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::config::{self, GlobalConfig, InstallMode};
use crate::error::Result;
use crate::git;
use crate::lockfile::{LockedPackage, LockedPackageKind, Lockfile};
use crate::manifest::{DependencySpec, Manifest, PluginDependencySpec};
use crate::nu;
use crate::resolver::{self, ResolvedDep, ResolvedPlugin};
use crate::safety;
use crate::ui;
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

#[derive(Debug, Clone, Copy)]
struct SecurityPolicy {
    require_signed_assets: bool,
    allow_unsigned: bool,
    no_build_fallback: bool,
}

#[derive(Debug, Default, Clone)]
struct DownloadVerificationMetadata {
    asset_sha256: Option<String>,
    asset_url: Option<String>,
}

#[derive(Debug, Clone)]
struct GitHubReleaseAssetCandidate {
    release_tag: String,
    release_assets: Vec<GitHubReleaseAsset>,
    asset: GitHubReleaseAsset,
}

/// Run a full local install: resolve -> fetch -> checksum -> place -> lock.
pub fn install(
    project_dir: &Path,
    frozen: bool,
    allow_unsigned: bool,
    no_build_fallback: bool,
) -> Result<()> {
    let manifest = Manifest::from_dir(project_dir)?;
    let global_config = GlobalConfig::load_or_default()?;
    let install_mode = global_config.install_mode;
    let security_policy = security_policy_for(
        global_config.security.require_signed_assets,
        frozen,
        allow_unsigned,
        no_build_fallback,
    );
    let lock_path = project_dir.join("quiver.lock");
    let nu_env_dir = project_dir.join(NU_ENV_DIR);
    let modules_dir = nu_env_dir.join(MODULES_SUBDIR);
    let bin_dir = nu_env_dir.join(BIN_SUBDIR);
    let display_name = format!("{NU_ENV_DIR}/{MODULES_SUBDIR}");

    if manifest.dependencies.is_empty() {
        ui::warn("No dependencies declared in nupackage.toml.");
        write_config_nu(&nu_env_dir, &modules_dir)?;
        create_nu_symlink_with_policy(
            &nu_env_dir,
            manifest.package.nu_version.as_deref(),
            security_policy,
        )?;
        write_activate_overlay(&nu_env_dir, project_dir)?;
        return Ok(());
    }

    // Determine whether to re-resolve or use the lockfile
    let mut frozen_lockfile: Option<Lockfile> = None;
    let (resolved_modules, resolved_plugins) = if frozen {
        // --frozen: use lockfile only
        if !lock_path.exists() {
            return Err(crate::error::QuiverError::Lockfile(
                "quiver.lock not found (required with --frozen)".to_string(),
            ));
        }
        let lockfile = Lockfile::from_path(&lock_path)?;
        frozen_lockfile = Some(lockfile.clone());
        ui::info(format!(
            "{} locked dependencies (--frozen)",
            ui::keyword("Using")
        ));
        (
            resolver::resolve_modules_from_lock(&lockfile.packages),
            resolver::resolve_plugins_from_lock(&lockfile.packages),
        )
    } else if lock_path.exists() {
        let lockfile = Lockfile::from_path(&lock_path)?;

        if !local_lockfile_is_stale(&manifest, &lockfile) {
            ui::info(format!("{} existing lockfile", ui::keyword("Using")));
            (
                resolver::resolve_modules_from_lock(&lockfile.packages),
                resolver::resolve_plugins_from_lock(&lockfile.packages),
            )
        } else {
            if !manifest.dependencies.modules.is_empty() {
                ui::info(format!("{} module dependencies", ui::keyword("Resolving")));
            }
            if !manifest.dependencies.plugins.is_empty() {
                ui::info(format!("{} plugin dependencies", ui::keyword("Resolving")));
            }
            (
                resolver::resolve_modules_from_deps(&manifest.dependencies.modules)?,
                resolver::resolve_plugins_from_deps(&manifest.dependencies.plugins)?,
            )
        }
    } else if manifest.dependencies.modules.is_empty() && manifest.dependencies.plugins.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        // No lockfile yet: resolve from manifest.
        ui::info(format!("{} module dependencies", ui::keyword("Resolving")));
        if !manifest.dependencies.plugins.is_empty() {
            ui::info(format!("{} plugin dependencies", ui::keyword("Resolving")));
        }
        (
            resolver::resolve_modules_from_deps(&manifest.dependencies.modules)?,
            resolver::resolve_plugins_from_deps(&manifest.dependencies.plugins)?,
        )
    };

    // Install each dependency
    install_resolved(
        &resolved_modules,
        &resolved_plugins,
        &modules_dir,
        &bin_dir,
        &lock_path,
        &nu_env_dir,
        manifest.package.nu_version.as_deref(),
        &display_name,
        install_mode,
        frozen,
        frozen_lockfile.as_ref(),
        security_policy,
    )
}

/// Run an update: always re-resolve, ignoring existing lockfile.
pub fn update(project_dir: &Path) -> Result<()> {
    let lock_path = project_dir.join("quiver.lock");
    // Remove existing lockfile to force re-resolution
    if lock_path.exists() {
        std::fs::remove_file(&lock_path)?;
    }
    install(project_dir, false, false, false)
}

/// Run a global install: resolve from `~/.config/quiver/config.toml` and install
/// modules to the configured global directory.
pub fn install_global(frozen: bool, allow_unsigned: bool, no_build_fallback: bool) -> Result<()> {
    let config = GlobalConfig::load()?;
    let install_mode = config.install_mode;
    let _security_policy = security_policy_for(
        config.security.require_signed_assets,
        frozen,
        allow_unsigned,
        no_build_fallback,
    );
    let modules_dir = config.modules_dir()?;
    let lock_path = config::global_lock_path()?;
    let display_dir = modules_dir.display().to_string();

    if config.dependencies.is_empty() {
        ui::warn("No dependencies declared in global config.");
        return Ok(());
    }

    let mut frozen_lockfile: Option<Lockfile> = None;
    let resolved_modules = if frozen {
        if !lock_path.exists() {
            return Err(crate::error::QuiverError::Lockfile(
                "config.lock not found (required with --frozen)".to_string(),
            ));
        }
        let lockfile = Lockfile::from_path(&lock_path)?;
        frozen_lockfile = Some(lockfile.clone());
        ui::info(format!(
            "{} locked global dependencies (--frozen)",
            ui::keyword("Using")
        ));
        resolver::resolve_modules_from_lock(&lockfile.packages)
    } else if lock_path.exists() && !is_global_lockfile_stale(&config, &lock_path)? {
        let lockfile = Lockfile::from_path(&lock_path)?;
        ui::info(format!("{} existing global lockfile", ui::keyword("Using")));
        resolver::resolve_modules_from_lock(&lockfile.packages)
    } else if config.dependencies.is_empty() {
        Vec::new()
    } else {
        ui::info(format!(
            "{} global module dependencies",
            ui::keyword("Resolving")
        ));
        resolver::resolve_modules_from_deps(&config.dependencies)?
    };

    install_resolved_global(
        &resolved_modules,
        &modules_dir,
        &lock_path,
        &display_dir,
        install_mode,
        frozen,
        frozen_lockfile.as_ref(),
    )
}

/// Install resolved global dependencies and write `config.lock`.
fn install_resolved_global(
    modules: &[ResolvedDep],
    modules_dir: &Path,
    lock_path: &Path,
    modules_display_name: &str,
    install_mode: InstallMode,
    frozen: bool,
    frozen_lockfile: Option<&Lockfile>,
) -> Result<()> {
    std::fs::create_dir_all(modules_dir)?;

    let mut locked_packages = Vec::new();

    for dep in modules {
        safety::validate_dependency_name(&dep.name, "module dependency")?;
        ui::info(format!(
            "{} module {}@{}",
            ui::keyword("Installing"),
            dep.name,
            &dep.rev[..12.min(dep.rev.len())]
        ));
        install_dep(dep, modules_dir, install_mode)?;

        let dest = modules_dir.join(&dep.name);
        let sha256 = resolver::compute_checksum(&dest)?;
        if frozen {
            verify_frozen_checksum(
                frozen_lockfile,
                &dep.name,
                LockedPackageKind::Module,
                &sha256,
            )?;
        }

        locked_packages.push(LockedPackage {
            name: dep.name.clone(),
            kind: LockedPackageKind::Module,
            git: dep.git.clone(),
            tag: dep.tag.clone(),
            rev: dep.rev.clone(),
            path: None,
            sha256,
            asset_sha256: None,
            asset_url: None,
        });
    }

    locked_packages.sort_by(|a, b| a.kind.cmp(&b.kind).then(a.name.cmp(&b.name)));

    if !frozen {
        let lockfile = Lockfile {
            version: 1,
            packages: locked_packages,
        };
        lockfile.write_to(lock_path)?;
    }

    let module_count = modules.len();
    let module_suffix = if module_count == 1 { "" } else { "s" };

    ui::success(format!(
        "Installed {module_count} module{module_suffix} into {modules_display_name}/"
    ));

    Ok(())
}

/// Install a list of resolved dependencies into a target directory and write the lockfile.
fn install_resolved(
    modules: &[ResolvedDep],
    plugins: &[ResolvedPlugin],
    modules_dir: &Path,
    bin_dir: &Path,
    lock_path: &Path,
    nu_env_dir: &Path,
    nu_version_req: Option<&str>,
    display_name: &str,
    install_mode: InstallMode,
    frozen: bool,
    frozen_lockfile: Option<&Lockfile>,
    security_policy: SecurityPolicy,
) -> Result<()> {
    std::fs::create_dir_all(modules_dir)?;
    std::fs::create_dir_all(bin_dir)?;
    let mut locked_packages = Vec::new();
    let existing_lockfile = if !frozen && lock_path.exists() {
        Lockfile::from_path(lock_path).ok()
    } else {
        None
    };

    for dep in modules {
        safety::validate_dependency_name(&dep.name, "module dependency")?;
        ui::info(format!(
            "{} module {}@{}",
            ui::keyword("Installing"),
            dep.name,
            &dep.rev[..12.min(dep.rev.len())]
        ));
        install_dep(dep, modules_dir, install_mode)?;

        let dest = modules_dir.join(&dep.name);
        let sha256 = resolver::compute_checksum(&dest)?;
        if frozen {
            verify_frozen_checksum(
                frozen_lockfile,
                &dep.name,
                LockedPackageKind::Module,
                &sha256,
            )?;
        }

        locked_packages.push(LockedPackage {
            name: dep.name.clone(),
            kind: LockedPackageKind::Module,
            git: dep.git.clone(),
            tag: dep.tag.clone(),
            rev: dep.rev.clone(),
            path: None,
            sha256,
            asset_sha256: None,
            asset_url: None,
        });
    }

    for plugin in plugins {
        safety::validate_dependency_name(&plugin.name, "plugin dependency")?;
        if let Some(bin) = plugin.bin.as_deref() {
            safety::validate_binary_name(bin, "plugin dependency bin")?;
        }
        ui::info(format!(
            "{} plugin {}@{}",
            ui::keyword("Installing"),
            plugin.name,
            &plugin.rev[..12.min(plugin.rev.len())]
        ));
        let frozen_locked_plugin = frozen_lockfile
            .and_then(|lock| lock.find_package(&plugin.name, LockedPackageKind::Plugin));
        let plugin_install = install_plugin(
            plugin,
            nu_version_req,
            security_policy,
            frozen_locked_plugin,
        )?;
        let installed_bin = plugin_install.installed_bin;
        let bin_name = plugin_install.bin_name;
        let version_dir = plugin_install.version_dir;
        link_plugin_into_env(&installed_bin, bin_dir, &bin_name)?;
        let sha256 = resolver::compute_checksum(&version_dir)?;
        if frozen {
            verify_frozen_checksum(
                frozen_lockfile,
                &plugin.name,
                LockedPackageKind::Plugin,
                &sha256,
            )?;
        }
        if frozen
            && let Some(expected_asset_sha) =
                frozen_locked_plugin.and_then(|locked| locked.asset_sha256.as_deref())
        {
            let actual_asset_sha = plugin_install.asset_metadata.asset_sha256.as_deref().ok_or_else(|| {
                crate::error::QuiverError::Lockfile(format!(
                    "frozen install requires downloaded asset_sha256 for plugin '{}' but no verified release asset digest was recorded",
                    plugin.name
                ))
            })?;
            if expected_asset_sha != actual_asset_sha {
                return Err(crate::error::QuiverError::Lockfile(format!(
                    "frozen install asset checksum mismatch for plugin '{}': expected {}, got {}",
                    plugin.name, expected_asset_sha, actual_asset_sha
                )));
            }
        }

        let existing_metadata = existing_lockfile
            .as_ref()
            .and_then(|lock| lock.find_package(&plugin.name, LockedPackageKind::Plugin));
        let asset_sha256 = plugin_install
            .asset_metadata
            .asset_sha256
            .or_else(|| existing_metadata.and_then(|pkg| pkg.asset_sha256.clone()));
        let asset_url = plugin_install
            .asset_metadata
            .asset_url
            .or_else(|| existing_metadata.and_then(|pkg| pkg.asset_url.clone()));

        let locked_tag = if plugin.git == "nu-core" {
            None
        } else {
            plugin.tag.clone()
        };
        let locked_rev = if plugin.git == "nu-core" {
            version_dir
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("nu-core")
                .to_string()
        } else {
            plugin.rev.clone()
        };

        locked_packages.push(LockedPackage {
            name: plugin.name.clone(),
            kind: LockedPackageKind::Plugin,
            git: plugin.git.clone(),
            tag: locked_tag,
            rev: locked_rev,
            path: Some(bin_name),
            sha256,
            asset_sha256,
            asset_url,
        });
    }

    locked_packages.sort_by(|a, b| a.kind.cmp(&b.kind).then(a.name.cmp(&b.name)));

    // Write lockfile
    if !frozen {
        let lockfile = Lockfile {
            version: 1,
            packages: locked_packages,
        };
        lockfile.write_to(lock_path)?;
    }

    let module_count = modules.len();
    let plugin_count = plugins.len();
    let module_suffix = if module_count == 1 { "" } else { "s" };
    let plugin_suffix = if plugin_count == 1 { "" } else { "s" };
    ui::success(format!(
        "Installed {module_count} module{module_suffix} and {plugin_count} plugin{plugin_suffix} into {display_name}/"
    ));

    // Derive project_dir from nu_env_dir (parent of .nu-env)
    let project_dir = nu_env_dir.parent().unwrap_or(nu_env_dir);

    write_config_nu(nu_env_dir, modules_dir)?;
    create_nu_symlink_with_policy(nu_env_dir, nu_version_req, security_policy)?;
    write_activate_overlay(nu_env_dir, project_dir)?;
    print_plugin_registration_instructions(plugins);

    Ok(())
}

fn print_plugin_registration_instructions(plugins: &[ResolvedPlugin]) {
    let commands = plugin_registration_commands(plugins);
    if commands.is_empty() {
        return;
    }

    ui::info("Run the following to finish enabling your new plugin(s) for this shell:");
    eprintln!(
        "  {}",
        ui::command_with_inline_comment("overlay use .nu-env/activate.nu # sets the nushell env")
    );
    eprintln!(
        "  {}",
        ui::command_with_inline_comment(
            "nu # runs the proper version of nu for this project with the module and plugin configs"
        )
    );
    for (plugin_name, use_name) in commands {
        eprintln!("  {}", ui::command(format!("plugin add {plugin_name}")));
        eprintln!("  {}", ui::command(format!("plugin use {use_name}")));
    }
}

fn plugin_registration_commands(plugins: &[ResolvedPlugin]) -> Vec<(String, String)> {
    let mut commands = Vec::new();
    let mut seen = HashSet::new();

    for plugin in plugins {
        let plugin_name = plugin
            .bin
            .clone()
            .unwrap_or_else(|| plugin.name.clone())
            .trim()
            .to_string();
        if plugin_name.is_empty() {
            continue;
        }

        if seen.insert(plugin_name.clone()) {
            commands.push((plugin_name.clone(), plugin_use_name(&plugin_name)));
        }
    }

    commands.sort_by(|a, b| a.0.cmp(&b.0));
    commands
}

fn plugin_use_name(plugin_name: &str) -> String {
    let base = plugin_name.strip_suffix(".exe").unwrap_or(plugin_name);
    base.strip_prefix("nu_plugin_").unwrap_or(base).to_string()
}

fn verify_frozen_checksum(
    frozen_lockfile: Option<&Lockfile>,
    name: &str,
    kind: LockedPackageKind,
    actual_sha256: &str,
) -> Result<()> {
    let lockfile = frozen_lockfile.ok_or_else(|| {
        crate::error::QuiverError::Lockfile(
            "internal error: frozen install missing loaded lockfile".to_string(),
        )
    })?;
    let expected = lockfile.find_package(name, kind.clone()).ok_or_else(|| {
        crate::error::QuiverError::Lockfile(format!(
            "frozen install expected package '{name}' ({kind:?}) in lockfile but it was missing"
        ))
    })?;
    if expected.sha256.trim().is_empty() {
        return Err(crate::error::QuiverError::Lockfile(format!(
            "frozen install requires non-empty sha256 for package '{name}' ({kind:?})"
        )));
    }
    if expected.sha256 != actual_sha256 {
        return Err(crate::error::QuiverError::Lockfile(format!(
            "checksum mismatch for package '{name}' ({kind:?}): expected {}, got {}",
            expected.sha256, actual_sha256
        )));
    }
    Ok(())
}

/// Generate `.nu-env/activate.nu` with a `nu` alias and deactivate alias.
pub fn write_activate_overlay(nu_env_dir: &Path, project_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(nu_env_dir)?;

    let nu_bin = nu_env_dir.join(BIN_SUBDIR).join("nu");
    let env_bin_dir = nu_env_dir.join(BIN_SUBDIR);
    let config_nu = nu_env_dir.join("config.nu");
    let plugin_config = nu_env_dir.join("plugins.msgpackz");
    let nu_bin_literal = nu_string_literal(&nu_bin);
    let env_bin_literal = nu_string_literal(&env_bin_dir);
    let config_nu_literal = nu_string_literal(&config_nu);
    let plugin_config_literal = nu_string_literal(&plugin_config);

    let activate_script = format!(
        r#"# Generated by quiver - do not edit
use std [ 'path add' ]

export-env {{
    source-env {config_nu_literal}
    path add {env_bin_literal}
}}

export alias nu = ^{nu_bin_literal} --config {config_nu_literal} --plugin-config {plugin_config_literal}

# deactivate the virtual environment for this project
export alias deactivate = overlay hide activate
"#,
    );

    let activate_path = nu_env_dir.join("activate.nu");
    std::fs::write(&activate_path, activate_script)?;
    ui::success(format!(
        "Generated {}",
        activate_path
            .strip_prefix(project_dir)
            .unwrap_or(&activate_path)
            .display()
    ));
    Ok(())
}

/// Generate `.nu-env/config.nu` with `NU_LIB_DIRS` pointing to the modules directory.
pub fn write_config_nu(nu_env_dir: &Path, modules_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(nu_env_dir)?;

    let modules_literal = nu_string_literal(modules_dir);
    let bin_literal = nu_string_literal(&nu_env_dir.join(BIN_SUBDIR));
    let nu_bin_literal = nu_string_literal(&nu_env_dir.join(BIN_SUBDIR).join("nu"));
    let plugin_config_literal = nu_string_literal(&nu_env_dir.join("plugins.msgpackz"));
    let config_literal = nu_string_literal(&nu_env_dir.join("config.nu"));

    let env_script = format!(
        r#"# Generated by quiver - do not edit
use std [ 'path add' ]

export-env {{
    path add {bin_literal}
}}

export const NU_LIB_DIRS = [
    {modules_literal}
]

export const NU_PLUGIN_DIRS = [
    {bin_literal}
]

$env.NU_LIB_DIRS = [
    {modules_literal}
]

export alias nu = ^{nu_bin_literal} --config {config_literal} --plugin-config {plugin_config_literal}
"#,
    );

    let config_path = nu_env_dir.join("config.nu");
    std::fs::write(&config_path, env_script)?;
    ui::success(format!("Generated {}", config_path.display()));
    Ok(())
}

/// Create a symlink at `.nu-env/bin/nu` pointing to a matching `nu` binary.
///
/// Resolution order:
/// 1. `nu` in PATH (if present and matches `nu-version` when provided)
/// 2. `~/.local/share/quiver/installs/nu_versions/<version>/` store
pub fn create_nu_symlink(nu_env_dir: &Path, nu_version_req: Option<&str>) -> Result<()> {
    create_nu_symlink_with_policy(
        nu_env_dir,
        nu_version_req,
        SecurityPolicy {
            require_signed_assets: true,
            allow_unsigned: false,
            no_build_fallback: false,
        },
    )
}

fn create_nu_symlink_with_policy(
    nu_env_dir: &Path,
    nu_version_req: Option<&str>,
    security_policy: SecurityPolicy,
) -> Result<()> {
    let bin_dir = nu_env_dir.join(BIN_SUBDIR);
    std::fs::create_dir_all(&bin_dir)?;

    let symlink_path = bin_dir.join("nu");

    // Remove existing symlink if present
    if symlink_path.exists() || symlink_path.symlink_metadata().is_ok() {
        std::fs::remove_file(&symlink_path)?;
    }

    let nu_path = match resolve_nu_binary_for_requirement(nu_version_req, security_policy)? {
        Some(path) => path,
        None if nu_version_req.is_none() => {
            ui::warn("could not find 'nu' in PATH; skipping .nu-env/bin/nu symlink.");
            return Ok(());
        }
        None => {
            let required = nu_version_req.unwrap_or_default();
            let installs = config::installs_nu_versions_dir()?;
            return Err(crate::error::QuiverError::Other(format!(
                "could not find a Nushell binary matching '{required}' in PATH or {}; install it under {}/<version>/nu",
                installs.display(),
                installs.display()
            )));
        }
    };

    #[cfg(unix)]
    std::os::unix::fs::symlink(&nu_path, &symlink_path)?;
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&nu_path, &symlink_path)?;

    ui::success(format!("Linked .nu-env/bin/nu -> {}", nu_path.display()));
    Ok(())
}

fn resolve_nu_binary_for_requirement(
    nu_version_req: Option<&str>,
    security_policy: SecurityPolicy,
) -> Result<Option<PathBuf>> {
    let requirement = nu_version_req
        .map(|raw| {
            nu::parse_nu_version_requirement(raw).map_err(|err| {
                crate::error::QuiverError::Manifest(format!(
                    "package nu-version '{raw}' is invalid: {err}"
                ))
            })
        })
        .transpose()?;
    resolve_nu_binary(requirement.as_ref(), security_policy)
}

fn resolve_nu_binary(
    requirement: Option<&VersionReq>,
    security_policy: SecurityPolicy,
) -> Result<Option<PathBuf>> {
    if let Some(nu_path) = detect_nu_path() {
        if let Some(req) = requirement {
            if detect_nu_binary_version(&nu_path).is_some_and(|v| req.matches(&v)) {
                return Ok(Some(nu_path));
            }
        } else {
            return Ok(Some(nu_path));
        }
    }

    let mut candidates = discover_installed_nu_binaries()?;

    if let Some(req) = requirement {
        candidates.retain(|candidate| req.matches(&candidate.version));
    }

    if let Some(selected) = candidates
        .into_iter()
        .max_by(|a, b| a.version.cmp(&b.version))
    {
        return Ok(Some(selected.path));
    }

    if let Some(req) = requirement {
        let target = select_nu_version_to_install(req)?;
        let path = install_nu_version_from_github_release(&target, security_policy)?;
        return Ok(Some(path));
    }

    Ok(None)
}

fn core_plugin_candidate_paths(nu_path: &Path, binary_filename: &str) -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(nu_bin_dir) = nu_path.parent() {
        candidates.push(nu_bin_dir.join(binary_filename));
        if nu_bin_dir.file_name().and_then(|name| name.to_str()) == Some("bin")
            && let Some(version_dir) = nu_bin_dir.parent()
        {
            candidates.push(version_dir.join("plugins").join(binary_filename));
        }
    }

    if let Some(nu_version) = detect_nu_binary_version(nu_path)
        && let Ok(plugins_root) = config::installs_plugins_dir()
    {
        let plugin_name = binary_filename
            .strip_suffix(".exe")
            .unwrap_or(binary_filename);
        candidates.push(
            plugins_root
                .join(plugin_name)
                .join(format!("nu-{nu_version}"))
                .join("bin")
                .join(binary_filename),
        );
    }

    candidates
}

fn find_core_plugin_binary_for_nu(nu_path: &Path, binary_filename: &str) -> Option<PathBuf> {
    core_plugin_candidate_paths(nu_path, binary_filename)
        .into_iter()
        .find(|candidate| candidate.is_file())
}

/// Detect the absolute path to the user's `nu` binary from PATH.
fn detect_nu_path() -> Option<PathBuf> {
    let output = if cfg!(windows) {
        Command::new("where").arg("nu").output().ok()?
    } else {
        Command::new("which").arg("nu").output().ok()?
    };
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let path_str = stdout.lines().next()?.trim();
    if path_str.is_empty() {
        None
    } else {
        Some(PathBuf::from(path_str))
    }
}

fn detect_nu_binary_version(path: &Path) -> Option<Version> {
    let output = Command::new(path).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    nu::extract_semver_from_text(&stdout)
}

#[derive(Debug, Clone)]
struct NuBinaryCandidate {
    path: PathBuf,
    version: Version,
}

fn discover_installed_nu_binaries() -> Result<Vec<NuBinaryCandidate>> {
    let nu_versions_dir = config::installs_nu_versions_dir()?;
    if !nu_versions_dir.exists() {
        return Ok(Vec::new());
    }

    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(&nu_versions_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }

        let version_name = entry.file_name();
        let version_name = version_name.to_string_lossy();
        let Ok(version) = Version::parse(&version_name) else {
            continue;
        };

        if let Some(path) = find_nu_binary_in_version_dir(&entry.path()) {
            candidates.push(NuBinaryCandidate { path, version });
        }
    }

    Ok(candidates)
}

fn find_nu_binary_in_version_dir(version_dir: &Path) -> Option<PathBuf> {
    let binary_name = if cfg!(windows) { "nu.exe" } else { "nu" };
    let direct = version_dir.join(binary_name);
    if direct.is_file() {
        return Some(direct);
    }

    let in_bin_dir = version_dir.join("bin").join(binary_name);
    if in_bin_dir.is_file() {
        return Some(in_bin_dir);
    }

    None
}

#[derive(Debug, Clone, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GitHubReleaseAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct GitHubReleaseAsset {
    name: String,
    #[serde(default)]
    browser_download_url: String,
}

fn select_nu_version_to_install(requirement: &VersionReq) -> Result<Version> {
    if let Some(exact) = exact_required_version(requirement) {
        return Ok(exact);
    }

    let releases = fetch_nu_github_releases()?;
    let expected_asset_suffix = nu_release_asset_suffix()?;

    let mut candidates = Vec::new();
    for release in releases {
        if release.draft || release.prerelease {
            continue;
        }
        let Some(version) = parse_version_from_release_tag(&release.tag_name) else {
            continue;
        };
        if !requirement.matches(&version) {
            continue;
        }

        let asset_name = format!("nu-{version}-{expected_asset_suffix}");
        if release.assets.iter().any(|asset| asset.name == asset_name) {
            candidates.push(version);
        }
    }

    candidates.into_iter().max().ok_or_else(|| {
        crate::error::QuiverError::Other(format!(
            "could not find a Nushell GitHub release matching requirement '{requirement}' for target '{}'",
            nu_release_target_triple().unwrap_or_else(|_| "unknown".to_string())
        ))
    })
}

fn exact_required_version(requirement: &VersionReq) -> Option<Version> {
    if requirement.comparators.len() != 1 {
        return None;
    }

    let comparator = &requirement.comparators[0];
    if comparator.op != semver::Op::Exact
        || comparator.minor.is_none()
        || comparator.patch.is_none()
        || !comparator.pre.is_empty()
    {
        return None;
    }

    Some(Version {
        major: comparator.major,
        minor: comparator.minor?,
        patch: comparator.patch?,
        pre: semver::Prerelease::EMPTY,
        build: semver::BuildMetadata::EMPTY,
    })
}

fn fetch_nu_github_releases() -> Result<Vec<GitHubRelease>> {
    ui::info(format!(
        "{} Nushell release metadata from GitHub",
        ui::keyword("Querying")
    ));
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "User-Agent: quiver",
            "https://api.github.com/repos/nushell/nushell/releases?per_page=100",
        ])
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(crate::error::QuiverError::Other(format!(
            "failed to query Nushell releases from GitHub: {stderr}"
        )));
    }

    serde_json::from_slice(&output.stdout).map_err(|err| {
        crate::error::QuiverError::Other(format!("invalid GitHub release response: {err}"))
    })
}

fn parse_version_from_release_tag(tag: &str) -> Option<Version> {
    let trimmed = tag.trim().trim_start_matches('v');
    Version::parse(trimmed).ok()
}

fn download_file_with_progress(url: &str, output_path: &Path, label: &str) -> Result<()> {
    ui::info(format!("{} {label}", ui::keyword("Downloading")));
    let progress = if let Some(total_bytes) = probe_content_length(url) {
        let pb = ui::bytes_progress(format!("downloading {label}"));
        pb.set_length(total_bytes);
        pb
    } else {
        ui::bytes_progress_unknown(format!("downloading {label}"))
    };

    let mut child = Command::new("curl")
        .args(["-fL", "-H", "User-Agent: quiver"])
        .arg(url)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let mut stdout = child.stdout.take().ok_or_else(|| {
        crate::error::QuiverError::Other("failed to capture curl output stream".to_string())
    })?;
    let mut file = std::fs::File::create(output_path)?;
    let mut buffer = [0_u8; 64 * 1024];

    loop {
        let read = stdout.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])?;
        progress.inc(read as u64);
    }

    let status = child.wait()?;

    if status.success() {
        progress.finish_and_clear();
        ui::success(format!("Downloaded {label} -> {}", output_path.display()));
        return Ok(());
    }

    progress.finish_and_clear();
    let _ = std::fs::remove_file(output_path);

    Err(crate::error::QuiverError::Other(format!(
        "curl download failed for {label} (exit status: {status})"
    )))
}

fn probe_content_length(url: &str) -> Option<u64> {
    let output = Command::new("curl")
        .args(["-fsSLI", "-H", "User-Agent: quiver"])
        .arg(url)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().rev().find_map(|line| {
        let mut parts = line.splitn(2, ':');
        let key = parts.next()?.trim();
        if !key.eq_ignore_ascii_case("content-length") {
            return None;
        }
        let value = parts.next()?.trim();
        value.parse::<u64>().ok()
    })
}

fn security_policy_for(
    config_requires_signed_assets: bool,
    frozen: bool,
    allow_unsigned_flag: bool,
    no_build_fallback_flag: bool,
) -> SecurityPolicy {
    if frozen {
        return SecurityPolicy {
            require_signed_assets: true,
            allow_unsigned: false,
            no_build_fallback: true,
        };
    }

    let require_signed_assets = if allow_unsigned_flag {
        false
    } else {
        config_requires_signed_assets
    };

    SecurityPolicy {
        require_signed_assets,
        allow_unsigned: allow_unsigned_flag,
        no_build_fallback: no_build_fallback_flag,
    }
}

fn is_asset_verification_error(err: &crate::error::QuiverError) -> bool {
    matches!(
        err,
        crate::error::QuiverError::ChecksumSourceNotFound { .. }
            | crate::error::QuiverError::ChecksumParse { .. }
            | crate::error::QuiverError::ChecksumMismatch { .. }
    )
}

fn download_text(url: &str, label: &str) -> Result<String> {
    ui::info(format!("{} {label}", ui::keyword("Downloading")));
    let output = Command::new("curl")
        .args(["-fsSL", "-H", "User-Agent: quiver"])
        .arg(url)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(crate::error::QuiverError::Other(format!(
            "failed to download {label}: {stderr}"
        )));
    }
    String::from_utf8(output.stdout).map_err(|err| {
        crate::error::QuiverError::Other(format!("invalid UTF-8 while downloading {label}: {err}"))
    })
}

fn download_release_checksums(
    release_assets: &[GitHubReleaseAsset],
    asset_name: &str,
    release_label: &str,
) -> Result<String> {
    let (source_asset, source_kind) = select_checksum_asset(release_assets, asset_name).ok_or_else(|| {
        crate::error::QuiverError::ChecksumSourceNotFound {
            asset: asset_name.to_string(),
            details: format!(
                "no checksum asset found in {release_label}; expected SHA256SUMS/checksums.txt or {}.sha256",
                asset_name
            ),
        }
    })?;
    if source_asset.browser_download_url.trim().is_empty() {
        return Err(crate::error::QuiverError::ChecksumSourceNotFound {
            asset: asset_name.to_string(),
            details: format!(
                "checksum asset '{}' in {release_label} had no download URL",
                source_asset.name
            ),
        });
    }
    download_text(
        &source_asset.browser_download_url,
        &format!("{source_kind} checksum source {}", source_asset.name),
    )
}

fn select_checksum_asset<'a>(
    release_assets: &'a [GitHubReleaseAsset],
    asset_name: &str,
) -> Option<(&'a GitHubReleaseAsset, &'static str)> {
    let preferred = release_assets.iter().find(|asset| {
        let lower = asset.name.to_ascii_lowercase();
        lower == "sha256sums"
            || lower == "sha256sums.txt"
            || lower == "checksums.txt"
            || lower == "checksums"
    });
    if let Some(asset) = preferred {
        return Some((asset, "multi-file"));
    }

    let fallback_name = format!("{}.sha256", asset_name.to_ascii_lowercase());
    release_assets
        .iter()
        .find(|asset| asset.name.to_ascii_lowercase() == fallback_name)
        .map(|asset| (asset, "single-file"))
}

fn parse_expected_sha256(checksum_text: &str, asset_name: &str) -> Result<String> {
    let mut lone_digest: Option<String> = None;

    for raw_line in checksum_text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some(hash) = parse_bsd_sha256_line(line, asset_name)? {
            return Ok(hash);
        }
        if let Some(hash) = parse_posix_sha256_line(line, asset_name)? {
            return Ok(hash);
        }
        if let Some(hash) = parse_lone_sha256_line(line, asset_name)? {
            lone_digest = Some(hash);
        }
    }

    lone_digest.ok_or_else(|| crate::error::QuiverError::ChecksumParse {
        asset: asset_name.to_string(),
        details: "checksum data did not contain a valid SHA-256 entry for the asset".to_string(),
    })
}

fn parse_bsd_sha256_line(line: &str, asset_name: &str) -> Result<Option<String>> {
    let prefix = "SHA256 (";
    if !line.starts_with(prefix) {
        return Ok(None);
    }
    let Some(end_name) = line.find(") =") else {
        return Err(crate::error::QuiverError::ChecksumParse {
            asset: asset_name.to_string(),
            details: format!("malformed BSD checksum line: '{line}'"),
        });
    };
    let filename = &line[prefix.len()..end_name];
    if filename != asset_name {
        return Ok(None);
    }
    let digest = line[end_name + 4..].trim();
    let normalized =
        normalize_sha256(digest).ok_or_else(|| crate::error::QuiverError::ChecksumParse {
            asset: asset_name.to_string(),
            details: format!("invalid SHA-256 digest for asset '{asset_name}': '{digest}'"),
        })?;
    Ok(Some(normalized))
}

fn parse_posix_sha256_line(line: &str, asset_name: &str) -> Result<Option<String>> {
    let mut parts = line.split_whitespace();
    let Some(candidate_digest) = parts.next() else {
        return Ok(None);
    };
    let Some(mut filename) = parts.next() else {
        return Ok(None);
    };

    if filename.starts_with('*') {
        filename = &filename[1..];
    }
    if filename != asset_name {
        return Ok(None);
    }

    let normalized = normalize_sha256(candidate_digest).ok_or_else(|| {
        crate::error::QuiverError::ChecksumParse {
            asset: asset_name.to_string(),
            details: format!(
                "invalid SHA-256 digest for asset '{asset_name}': '{candidate_digest}'"
            ),
        }
    })?;
    Ok(Some(normalized))
}

fn parse_lone_sha256_line(line: &str, asset_name: &str) -> Result<Option<String>> {
    if let Some(normalized) = normalize_sha256(line) {
        return Ok(Some(normalized));
    }

    if line.chars().all(|c| c.is_ascii_hexdigit()) && line.len() != 64 {
        return Err(crate::error::QuiverError::ChecksumParse {
            asset: asset_name.to_string(),
            details: format!("invalid SHA-256 digest length: '{}'", line.len()),
        });
    }

    Ok(None)
}

fn normalize_sha256(raw: &str) -> Option<String> {
    let digest = raw.trim().to_ascii_lowercase();
    if digest.len() != 64 {
        return None;
    }
    if digest.chars().all(|ch| ch.is_ascii_hexdigit()) {
        Some(digest)
    } else {
        None
    }
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn verify_downloaded_asset(path: &Path, asset_name: &str, expected_sha256: &str) -> Result<()> {
    let expected = normalize_sha256(expected_sha256).ok_or_else(|| {
        crate::error::QuiverError::ChecksumParse {
            asset: asset_name.to_string(),
            details: format!(
                "expected SHA-256 digest for asset '{}' was malformed: '{}'",
                asset_name, expected_sha256
            ),
        }
    })?;
    let actual = sha256_file(path)?;
    if actual != expected {
        return Err(crate::error::QuiverError::ChecksumMismatch {
            asset: asset_name.to_string(),
            expected_sha256: expected,
            actual_sha256: actual,
        });
    }
    Ok(())
}

fn install_nu_version_from_github_release(
    version: &Version,
    security_policy: SecurityPolicy,
) -> Result<PathBuf> {
    let version_dir = config::installs_nu_versions_dir()?.join(version.to_string());
    let binary_name = if cfg!(windows) { "nu.exe" } else { "nu" };
    let installed_path = version_dir.join("bin").join(binary_name);
    if installed_path.is_file() {
        return Ok(installed_path);
    }

    std::fs::create_dir_all(&version_dir)?;

    let asset_suffix = nu_release_asset_suffix()?;
    let asset_name = format!("nu-{version}-{asset_suffix}");
    let releases = fetch_nu_github_releases()?;
    let selected_release = releases
        .into_iter()
        .find(|release| {
            !release.draft
                && !release.prerelease
                && parse_version_from_release_tag(&release.tag_name).as_ref() == Some(version)
                && release.assets.iter().any(|asset| asset.name == asset_name)
        })
        .ok_or_else(|| {
            crate::error::QuiverError::Other(format!(
                "could not find Nushell release asset '{}' for version {}",
                asset_name, version
            ))
        })?;
    let asset = selected_release
        .assets
        .iter()
        .find(|candidate| candidate.name == asset_name)
        .cloned()
        .ok_or_else(|| {
            crate::error::QuiverError::Other(format!(
                "could not locate Nushell release asset '{}' in release {}",
                asset_name, selected_release.tag_name
            ))
        })?;
    if asset.browser_download_url.trim().is_empty() {
        return Err(crate::error::QuiverError::Other(format!(
            "Nushell release asset '{}' did not include a download URL",
            asset.name
        )));
    }

    let temp_root = config::installs_root_dir()?.join(format!(
        ".nu-download-{}-{}",
        version,
        std::process::id()
    ));
    if temp_root.exists() {
        let _ = std::fs::remove_dir_all(&temp_root);
    }
    std::fs::create_dir_all(&temp_root)?;

    let archive_path = temp_root.join(&asset_name);
    if let Err(err) = download_file_with_progress(
        &asset.browser_download_url,
        &archive_path,
        &format!("Nushell {version} release artifact"),
    ) {
        let _ = std::fs::remove_dir_all(&temp_root);
        return Err(crate::error::QuiverError::Other(format!(
            "failed to download Nushell {} release artifact '{}' from GitHub: {}",
            version, asset_name, err
        )));
    }

    let verification_result: Result<()> = (|| {
        let checksum_text = download_release_checksums(
            &selected_release.assets,
            &asset_name,
            &format!("Nushell release {}", selected_release.tag_name),
        )?;
        let expected_sha256 = parse_expected_sha256(&checksum_text, &asset_name)?;
        verify_downloaded_asset(&archive_path, &asset_name, &expected_sha256)?;
        Ok(())
    })();
    if let Err(err) = verification_result {
        if security_policy.require_signed_assets {
            let _ = std::fs::remove_dir_all(&temp_root);
            return Err(err);
        }
        ui::warn(format!(
            "SECURITY WARNING: Nushell release asset '{}' could not be verified ({err}); continuing because --allow-unsigned/config override is active",
            asset_name
        ));
    }

    let extract_dir = temp_root.join("extract");
    std::fs::create_dir_all(&extract_dir)?;
    extract_archive(&archive_path, &extract_dir)?;

    let extracted_binary = find_extracted_nu_binary(&extract_dir).ok_or_else(|| {
        crate::error::QuiverError::Other(format!(
            "downloaded Nushell archive did not contain '{}'",
            binary_name
        ))
    })?;

    let target_bin_dir = version_dir.join("bin");
    std::fs::create_dir_all(&target_bin_dir)?;
    std::fs::copy(&extracted_binary, &installed_path)?;
    make_executable(&installed_path)?;
    let plugins_root = config::installs_plugins_dir()?;
    let installed_plugins = install_extracted_nu_plugins(&extract_dir, &plugins_root, version)?;
    let _ = std::fs::remove_dir_all(&temp_root);

    if installed_plugins == 0 {
        ui::success(format!(
            "Installed Nushell {} from GitHub releases into {}",
            version,
            version_dir.display()
        ));
    } else {
        ui::success(format!(
            "Installed Nushell {} from GitHub releases into {} ({} bundled plugin{})",
            version,
            version_dir.display(),
            installed_plugins,
            if installed_plugins == 1 { "" } else { "s" }
        ));
    }

    Ok(installed_path)
}

fn nu_release_target_triple() -> Result<String> {
    let triple = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("windows", "aarch64") => "aarch64-pc-windows-msvc",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        (os, arch) => {
            return Err(crate::error::QuiverError::Other(format!(
                "unsupported platform for Nushell release downloads: {os}/{arch}"
            )));
        }
    };
    Ok(triple.to_string())
}

fn nu_release_asset_suffix() -> Result<String> {
    let triple = nu_release_target_triple()?;
    if cfg!(windows) {
        Ok(format!("{triple}.zip"))
    } else {
        Ok(format!("{triple}.tar.gz"))
    }
}

fn extract_archive(archive_path: &Path, extract_dir: &Path) -> Result<()> {
    let archive_name = archive_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();

    if archive_name.ends_with(".tar.gz") {
        let output = Command::new("tar")
            .arg("-xzf")
            .arg(archive_path)
            .arg("-C")
            .arg(extract_dir)
            .output()?;
        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(crate::error::QuiverError::Other(format!(
            "failed to extract Nushell tar archive: {stderr}"
        )));
    }

    if archive_name.ends_with(".zip") {
        let output = Command::new("unzip")
            .args(["-q"])
            .arg(archive_path)
            .arg("-d")
            .arg(extract_dir)
            .output()?;
        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(crate::error::QuiverError::Other(format!(
            "failed to extract Nushell zip archive: {stderr}"
        )));
    }

    Err(crate::error::QuiverError::Other(format!(
        "unsupported Nushell archive format: {}",
        archive_path.display()
    )))
}

fn find_extracted_nu_binary(extract_dir: &Path) -> Option<PathBuf> {
    let binary_name = if cfg!(windows) { "nu.exe" } else { "nu" };
    for entry in WalkDir::new(extract_dir)
        .follow_links(false)
        .into_iter()
        .filter_map(std::result::Result::ok)
    {
        if entry.file_type().is_file() && entry.file_name().to_string_lossy() == binary_name {
            return Some(entry.into_path());
        }
    }
    None
}

fn install_extracted_nu_plugins(
    extract_dir: &Path,
    plugins_root: &Path,
    version: &Version,
) -> Result<usize> {
    let plugins = find_extracted_nu_plugins(extract_dir);
    if plugins.is_empty() {
        return Ok(0);
    }

    for plugin in &plugins {
        let plugin_name = plugin.file_name().ok_or_else(|| {
            crate::error::QuiverError::Other(format!(
                "failed to determine bundled plugin file name for {}",
                plugin.display()
            ))
        })?;
        let plugin_name_str = plugin_name.to_string_lossy();
        let plugin_key = plugin_name_str
            .strip_suffix(".exe")
            .unwrap_or(&plugin_name_str);
        let target = plugins_root
            .join(plugin_key)
            .join(format!("nu-{version}"))
            .join("bin")
            .join(plugin_name);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(plugin, &target)?;
        make_executable(&target)?;
    }

    Ok(plugins.len())
}

fn find_extracted_nu_plugins(extract_dir: &Path) -> Vec<PathBuf> {
    let prefix = "nu_plugin_";
    let suffix = if cfg!(windows) { ".exe" } else { "" };

    let mut plugins = Vec::new();
    for entry in WalkDir::new(extract_dir)
        .follow_links(false)
        .into_iter()
        .filter_map(std::result::Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }

        let name = entry.file_name().to_string_lossy();
        if !name.starts_with(prefix) {
            continue;
        }
        if !suffix.is_empty() && !name.ends_with(suffix) {
            continue;
        }
        plugins.push(entry.into_path());
    }

    plugins.sort();
    plugins
}

fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions)?;
    }
    let _ = path;
    Ok(())
}

#[derive(Debug, Clone)]
struct PluginInstallResult {
    installed_bin: PathBuf,
    bin_name: String,
    version_dir: PathBuf,
    asset_metadata: DownloadVerificationMetadata,
}

fn install_plugin(
    plugin: &ResolvedPlugin,
    nu_version_req: Option<&str>,
    security_policy: SecurityPolicy,
    frozen_locked_plugin: Option<&LockedPackage>,
) -> Result<PluginInstallResult> {
    safety::validate_dependency_name(&plugin.name, "plugin dependency")?;
    if plugin.git == "nu-core" {
        return install_core_plugin(plugin, nu_version_req);
    }

    let bin_name = plugin.bin.clone().unwrap_or_else(|| plugin.name.clone());
    safety::validate_binary_name(&bin_name, "plugin dependency bin")?;
    let binary_filename = plugin_binary_filename(&bin_name);
    let version = plugin.tag.clone().unwrap_or_else(|| plugin.rev.clone());
    let version_dir = config::installs_plugins_dir()?
        .join(&plugin.name)
        .join(version);
    let installed_bin = version_dir.join("bin").join(&binary_filename);
    if installed_bin.is_file() {
        return Ok(PluginInstallResult {
            installed_bin,
            bin_name: binary_filename,
            version_dir,
            asset_metadata: DownloadVerificationMetadata::default(),
        });
    }

    std::fs::create_dir_all(version_dir.join("bin"))?;

    let asset_metadata = match install_plugin_from_github_release(
        plugin,
        &version_dir,
        &binary_filename,
        security_policy,
        frozen_locked_plugin,
    ) {
        Ok(metadata) => metadata,
        Err(err) => {
            if security_policy.no_build_fallback {
                return Err(err);
            }
            let verification_failure = is_asset_verification_error(&err);
            if verification_failure && !security_policy.allow_unsigned {
                return Err(err);
            }
            if verification_failure {
                ui::warn(format!(
                    "SECURITY WARNING: release verification failed for plugin '{}' ({err}); continuing with local cargo source build because --allow-unsigned was set",
                    plugin.name
                ));
            } else {
                ui::warn(format!(
                    "release install for plugin '{}' failed ({err}); falling back to cargo build",
                    plugin.name
                ));
            }
            ensure_cargo_available(&plugin.name)?;
            if !prompt_for_cargo_fallback_approval(plugin, &err)? {
                return Err(crate::error::QuiverError::Other(format!(
                    "release install for plugin '{}' failed and cargo fallback build was denied by user",
                    plugin.name
                )));
            }
            ui::warn(format!(
                "SECURITY WARNING: building plugin '{}' from source locally (trust mode changed)",
                plugin.name
            ));
            install_plugin_from_cargo_source(plugin, &version_dir, &binary_filename)?;
            DownloadVerificationMetadata::default()
        }
    };

    if !installed_bin.is_file() {
        return Err(crate::error::QuiverError::Other(format!(
            "plugin binary '{}' was not produced for {}",
            binary_filename, plugin.name
        )));
    }

    Ok(PluginInstallResult {
        installed_bin,
        bin_name: binary_filename,
        version_dir,
        asset_metadata,
    })
}

fn install_core_plugin(
    plugin: &ResolvedPlugin,
    nu_version_req: Option<&str>,
) -> Result<PluginInstallResult> {
    safety::validate_dependency_name(&plugin.name, "plugin dependency")?;
    let bin_name = plugin.bin.clone().unwrap_or_else(|| plugin.name.clone());
    safety::validate_binary_name(&bin_name, "plugin dependency bin")?;
    let binary_filename = plugin_binary_filename(&bin_name);

    let nu_path = resolve_nu_binary_for_requirement(
        nu_version_req,
        SecurityPolicy {
            require_signed_assets: true,
            allow_unsigned: false,
            no_build_fallback: false,
        },
    )?
    .ok_or_else(|| {
        crate::error::QuiverError::Other(
            "could not resolve a Nushell binary while installing core plugin".to_string(),
        )
    })?;
    let nu_version = detect_nu_binary_version(&nu_path).ok_or_else(|| {
        crate::error::QuiverError::Other(format!(
            "failed to detect Nushell version from {} while installing core plugin '{}'",
            nu_path.display(),
            plugin.name
        ))
    })?;
    let source_binary = find_core_plugin_binary_for_nu(&nu_path, &binary_filename).ok_or_else(|| {
        crate::error::QuiverError::Other(format!(
            "core plugin '{}' ({}) was not found next to '{}' or in quiver's shared plugin store",
            plugin.name,
            binary_filename,
            nu_path.display()
        ))
    })?;

    let version_dir = config::installs_plugins_dir()?
        .join(&plugin.name)
        .join(format!("nu-{nu_version}"));
    let installed_bin = version_dir.join("bin").join(&binary_filename);
    if installed_bin.is_file() {
        return Ok(PluginInstallResult {
            installed_bin,
            bin_name: binary_filename,
            version_dir,
            asset_metadata: DownloadVerificationMetadata::default(),
        });
    }

    std::fs::create_dir_all(version_dir.join("bin"))?;
    std::fs::copy(&source_binary, &installed_bin)?;
    make_executable(&installed_bin)?;

    Ok(PluginInstallResult {
        installed_bin,
        bin_name: binary_filename,
        version_dir,
        asset_metadata: DownloadVerificationMetadata::default(),
    })
}

fn plugin_binary_filename(bin_name: &str) -> String {
    if cfg!(windows) && !bin_name.ends_with(".exe") {
        format!("{bin_name}.exe")
    } else {
        bin_name.to_string()
    }
}

fn install_plugin_from_github_release(
    plugin: &ResolvedPlugin,
    version_dir: &Path,
    binary_filename: &str,
    security_policy: SecurityPolicy,
    _frozen_locked_plugin: Option<&LockedPackage>,
) -> Result<DownloadVerificationMetadata> {
    let (owner, repo) = parse_github_owner_repo(&plugin.git).ok_or_else(|| {
        crate::error::QuiverError::Other(format!(
            "plugin '{}' git source is not a supported GitHub repo URL",
            plugin.name
        ))
    })?;
    let releases = fetch_repo_github_releases(&owner, &repo)?;
    let selected = select_plugin_release_asset(&releases, plugin.tag.as_deref(), binary_filename)
        .ok_or_else(|| {
        crate::error::QuiverError::Other(format!(
            "no suitable release asset found for plugin '{}' ({}/{})",
            plugin.name, owner, repo
        ))
    })?;
    if selected.asset.browser_download_url.trim().is_empty() {
        return Err(crate::error::QuiverError::Other(format!(
            "release asset '{}' did not include a download URL",
            selected.asset.name
        )));
    }

    let temp_root = config::installs_root_dir()?.join(format!(
        ".plugin-download-{}-{}",
        plugin.name,
        std::process::id()
    ));
    if temp_root.exists() {
        let _ = std::fs::remove_dir_all(&temp_root);
    }
    std::fs::create_dir_all(&temp_root)?;

    let downloaded_path = temp_root.join(&selected.asset.name);
    if let Err(err) = download_file_with_progress(
        &selected.asset.browser_download_url,
        &downloaded_path,
        &format!("plugin {} asset {}", plugin.name, selected.asset.name),
    ) {
        let _ = std::fs::remove_dir_all(&temp_root);
        return Err(crate::error::QuiverError::Other(format!(
            "failed downloading plugin release asset '{}': {err}",
            selected.asset.name,
        )));
    }

    let mut asset_metadata = DownloadVerificationMetadata {
        asset_sha256: None,
        asset_url: Some(selected.asset.browser_download_url.clone()),
    };
    let verification_result: Result<String> = (|| {
        let checksum_text = download_release_checksums(
            &selected.release_assets,
            &selected.asset.name,
            &format!("plugin {} release {}", plugin.name, selected.release_tag),
        )?;
        let expected_sha256 = parse_expected_sha256(&checksum_text, &selected.asset.name)?;
        verify_downloaded_asset(&downloaded_path, &selected.asset.name, &expected_sha256)?;
        sha256_file(&downloaded_path)
    })();
    match verification_result {
        Ok(actual_sha256) => {
            asset_metadata.asset_sha256 = Some(actual_sha256);
        }
        Err(err) => {
            if security_policy.require_signed_assets {
                let _ = std::fs::remove_dir_all(&temp_root);
                return Err(err);
            }
            ui::warn(format!(
                "SECURITY WARNING: plugin '{}' release asset '{}' could not be verified ({err}); continuing because --allow-unsigned/config override is active",
                plugin.name, selected.asset.name
            ));
        }
    }

    let binary =
        if selected.asset.name.ends_with(".tar.gz") || selected.asset.name.ends_with(".zip") {
            let extract_dir = temp_root.join("extract");
            std::fs::create_dir_all(&extract_dir)?;
            extract_archive(&downloaded_path, &extract_dir)?;
            find_extracted_binary_named(&extract_dir, binary_filename).ok_or_else(|| {
                crate::error::QuiverError::Other(format!(
                    "release asset '{}' did not contain '{}'",
                    selected.asset.name, binary_filename
                ))
            })?
        } else {
            downloaded_path.clone()
        };

    let target = version_dir.join("bin").join(binary_filename);
    std::fs::copy(&binary, &target)?;
    make_executable(&target)?;
    let _ = std::fs::remove_dir_all(&temp_root);

    Ok(asset_metadata)
}

fn install_plugin_from_cargo_source(
    plugin: &ResolvedPlugin,
    version_dir: &Path,
    binary_filename: &str,
) -> Result<()> {
    let repo_path = git::clone_or_fetch(&plugin.git)?;
    let staging = config::installs_root_dir()?.join(format!(
        ".plugin-build-{}-{}",
        plugin.name,
        std::process::id()
    ));
    if staging.exists() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    std::fs::create_dir_all(&staging)?;
    git::export_to(&repo_path, &plugin.rev, &staging)?;

    let build_bin_name = plugin.bin.as_deref().unwrap_or(&plugin.name);
    let output = Command::new("cargo")
        .current_dir(&staging)
        .args(["build", "--release", "--bin", build_bin_name])
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let _ = std::fs::remove_dir_all(&staging);
        return Err(crate::error::QuiverError::Other(format!(
            "cargo fallback build failed for plugin '{}': {stderr}",
            plugin.name
        )));
    }

    let built_binary = staging.join("target").join("release").join(binary_filename);
    if !built_binary.is_file() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(crate::error::QuiverError::Other(format!(
            "cargo build succeeded but '{}' was not found for plugin '{}'",
            binary_filename, plugin.name
        )));
    }

    let target = version_dir.join("bin").join(binary_filename);
    std::fs::copy(&built_binary, &target)?;
    make_executable(&target)?;
    let _ = std::fs::remove_dir_all(&staging);
    Ok(())
}

fn ensure_cargo_available(plugin_name: &str) -> Result<()> {
    match Command::new("cargo")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => Ok(()),
        Ok(_) => Err(crate::error::QuiverError::Other(format!(
            "cargo is required for plugin '{}' source-build fallback, but `cargo --version` failed",
            plugin_name
        ))),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            Err(crate::error::QuiverError::Other(format!(
                "cargo is not installed or not on PATH; cannot source-build plugin '{}'",
                plugin_name
            )))
        }
        Err(err) => Err(err.into()),
    }
}

fn prompt_for_cargo_fallback_approval(
    plugin: &ResolvedPlugin,
    release_error: &crate::error::QuiverError,
) -> Result<bool> {
    eprintln!("  Release install failed with: {}", release_error);

    loop {
        eprint!(
            "{} Build plugin '{}' from source with `cargo build --release --bin {}`? [y/N]: ",
            ui::keyword("Confirm"),
            plugin.name,
            plugin.bin.as_deref().unwrap_or(&plugin.name)
        );
        std::io::stderr().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        match parse_yes_no(&input) {
            Some(answer) => return Ok(answer),
            None => eprintln!("Please answer 'y' or 'n'."),
        }
    }
}

fn parse_yes_no(input: &str) -> Option<bool> {
    match input.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Some(true),
        "n" | "no" | "" => Some(false),
        _ => None,
    }
}

fn link_plugin_into_env(
    installed_binary: &Path,
    bin_dir: &Path,
    binary_filename: &str,
) -> Result<()> {
    safety::validate_binary_name(binary_filename, "plugin binary filename")?;
    std::fs::create_dir_all(bin_dir)?;
    let link_path = bin_dir.join(binary_filename);
    if link_path.exists() || link_path.symlink_metadata().is_ok() {
        std::fs::remove_file(&link_path)?;
    }

    #[cfg(unix)]
    std::os::unix::fs::symlink(installed_binary, &link_path)?;
    #[cfg(windows)]
    {
        if let Err(err) = std::os::windows::fs::symlink_file(installed_binary, &link_path) {
            ui::warn(format!(
                "symlink failed ({err}); copying plugin binary instead"
            ));
            std::fs::copy(installed_binary, &link_path)?;
        }
    }

    ui::success(format!(
        "Linked .nu-env/bin/{} -> {}",
        binary_filename,
        installed_binary.display()
    ));
    Ok(())
}

fn parse_github_owner_repo(git_url: &str) -> Option<(String, String)> {
    let trimmed = git_url
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git");
    if let Some(rest) = trimmed
        .strip_prefix("https://github.com/")
        .or_else(|| trimmed.strip_prefix("http://github.com/"))
    {
        let mut parts = rest.split('/');
        let owner = parts.next()?;
        let repo = parts.next()?;
        if owner.is_empty() || repo.is_empty() || parts.next().is_some() {
            return None;
        }
        return Some((owner.to_string(), repo.to_string()));
    }

    if let Some(rest) = trimmed.strip_prefix("git@github.com:") {
        let mut parts = rest.split('/');
        let owner = parts.next()?;
        let repo = parts.next()?;
        if owner.is_empty() || repo.is_empty() || parts.next().is_some() {
            return None;
        }
        return Some((owner.to_string(), repo.to_string()));
    }

    None
}

fn fetch_repo_github_releases(owner: &str, repo: &str) -> Result<Vec<GitHubRelease>> {
    let url = format!("https://api.github.com/repos/{owner}/{repo}/releases?per_page=100");
    ui::info(format!(
        "{} release metadata for {owner}/{repo}",
        ui::keyword("Querying")
    ));
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "User-Agent: quiver",
        ])
        .arg(&url)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(crate::error::QuiverError::Other(format!(
            "failed to query plugin releases from GitHub: {stderr}"
        )));
    }

    serde_json::from_slice(&output.stdout).map_err(|err| {
        crate::error::QuiverError::Other(format!("invalid GitHub release response: {err}"))
    })
}

fn select_plugin_release_asset(
    releases: &[GitHubRelease],
    preferred_tag: Option<&str>,
    binary_filename: &str,
) -> Option<GitHubReleaseAssetCandidate> {
    for release in releases {
        if release.draft || release.prerelease {
            continue;
        }
        if let Some(tag) = preferred_tag {
            if !release_tag_matches(&release.tag_name, tag) {
                continue;
            }
        }

        let mut candidates: Vec<(i32, GitHubReleaseAsset)> = release
            .assets
            .iter()
            .filter_map(|asset| {
                let score = score_plugin_asset_name(&asset.name, binary_filename)?;
                Some((score, asset.clone()))
            })
            .collect();
        candidates.sort_by(|a, b| b.0.cmp(&a.0));
        if let Some((_, asset)) = candidates.into_iter().next() {
            return Some(GitHubReleaseAssetCandidate {
                release_tag: release.tag_name.clone(),
                release_assets: release.assets.clone(),
                asset,
            });
        }
    }
    None
}

fn release_tag_matches(actual: &str, expected: &str) -> bool {
    actual == expected || actual.trim_start_matches('v') == expected.trim_start_matches('v')
}

fn score_plugin_asset_name(asset_name: &str, binary_filename: &str) -> Option<i32> {
    let lower = asset_name.to_ascii_lowercase();
    let os_ok = match std::env::consts::OS {
        "macos" => lower.contains("darwin") || lower.contains("apple") || lower.contains("macos"),
        "linux" => lower.contains("linux"),
        "windows" => lower.contains("windows") || lower.contains("win"),
        _ => false,
    };
    if !os_ok {
        return None;
    }

    let arch_ok = match std::env::consts::ARCH {
        "aarch64" => lower.contains("aarch64") || lower.contains("arm64"),
        "x86_64" => lower.contains("x86_64") || lower.contains("amd64"),
        _ => false,
    };
    if !arch_ok {
        return None;
    }

    let mut score = 10;
    if lower.contains(&binary_filename.to_ascii_lowercase()) {
        score += 5;
    }
    if lower.ends_with(".tar.gz") || lower.ends_with(".zip") {
        score += 3;
    }
    if lower.ends_with(binary_filename) {
        score += 2;
    }

    Some(score)
}

fn find_extracted_binary_named(root: &Path, binary_filename: &str) -> Option<PathBuf> {
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(std::result::Result::ok)
    {
        if entry.file_type().is_file()
            && entry
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case(binary_filename)
        {
            return Some(entry.into_path());
        }
    }
    None
}

fn nu_string_literal(path: &Path) -> String {
    format!("{:?}", path.to_string_lossy())
}

/// Install a single resolved dependency into the modules directory.
fn install_dep(dep: &ResolvedDep, modules_dir: &Path, install_mode: InstallMode) -> Result<String> {
    safety::validate_dependency_name(&dep.name, "module dependency")?;
    let repo_path = git::clone_or_fetch(&dep.git)?;
    let dest = modules_dir.join(&dep.name);
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let staging = modules_dir.join(format!(".quiver-staging-{}-{unique}", std::process::id()));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }

    git::export_to(&repo_path, &dep.rev, &staging)?;
    materialize_module_dir(&staging, &dest, install_mode)?;
    std::fs::remove_dir_all(&staging)?;
    discover_module_use_path(&dest, &dep.name)
}

fn materialize_module_dir(src: &Path, dest: &Path, mode: InstallMode) -> Result<()> {
    if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }

    match mode {
        InstallMode::Clone => {
            if let Err(err) = clone_dir(src, dest) {
                ui::warn(format!("clone mode failed ({err}); falling back to copy"));
                copy_dir(src, dest)?;
            }
            Ok(())
        }
        InstallMode::Hardlink => hardlink_dir(src, dest),
        InstallMode::Copy => copy_dir(src, dest),
    }
}

fn clone_dir(src: &Path, dest: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        copy_with_cp(src, dest, &["-cR"])
    }

    #[cfg(target_os = "linux")]
    {
        copy_with_cp(src, dest, &["-a", "--reflink=always"])
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (src, dest);
        Err(crate::error::QuiverError::Other(
            "clone mode is not supported on this platform".to_string(),
        ))
    }
}

fn copy_with_cp(src: &Path, dest: &Path, flags: &[&str]) -> Result<()> {
    let output = Command::new("cp").args(flags).arg(src).arg(dest).output()?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let message = if stderr.is_empty() {
        "cp failed with unknown error".to_string()
    } else {
        stderr
    };
    Err(crate::error::QuiverError::Other(message))
}

fn hardlink_dir(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in WalkDir::new(src)
        .follow_links(false)
        .into_iter()
        .filter_map(std::result::Result::ok)
    {
        let relative = entry
            .path()
            .strip_prefix(src)
            .map_err(|e| crate::error::QuiverError::Other(e.to_string()))?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let target = dest.join(relative);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::hard_link(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn copy_dir(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in WalkDir::new(src)
        .follow_links(false)
        .into_iter()
        .filter_map(std::result::Result::ok)
    {
        let relative = entry
            .path()
            .strip_prefix(src)
            .map_err(|e| crate::error::QuiverError::Other(e.to_string()))?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let target = dest.join(relative);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &target)?;
        }
    }
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
        if let Some(subdir) = safety::normalized_relative_path(Path::new(package_name)) {
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

    ui::warn(format!(
        "could not locate mod.nu for module '{dep_name}' after install; defaulting to '{dep_name}'"
    ));
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

    let hint_path = safety::normalized_relative_path(Path::new(&normalized_hint))?;
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
            if let Some(normalized) = safety::normalized_relative_path(parent) {
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
    for (name, spec) in &manifest.dependencies.modules {
        let Some(locked) = lockfile.find_package(name, LockedPackageKind::Module) else {
            return true;
        };
        if !module_dep_matches_lock(spec, locked) {
            return true;
        }
    }
    for (name, spec) in &manifest.dependencies.plugins {
        let Some(locked) = lockfile.find_package(name, LockedPackageKind::Plugin) else {
            return true;
        };
        if !plugin_dep_matches_lock(name, spec, locked) {
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
            LockedPackageKind::Plugin => {
                if !manifest.dependencies.plugins.contains_key(&pkg.name) {
                    return true;
                }
            }
            LockedPackageKind::Other => return true,
        }
    }

    false
}

fn module_dep_matches_lock(spec: &DependencySpec, locked: &LockedPackage) -> bool {
    if spec.git != locked.git {
        return false;
    }
    if spec.tag != locked.tag {
        return false;
    }
    if let Some(expected_rev) = &spec.rev
        && expected_rev != &locked.rev
    {
        return false;
    }
    true
}

fn plugin_dep_matches_lock(name: &str, spec: &PluginDependencySpec, locked: &LockedPackage) -> bool {
    let source = spec.source.as_deref().unwrap_or("git");
    if source == "nu-core" {
        if locked.git != "nu-core" {
            return false;
        }
    } else if spec.git != locked.git {
        return false;
    }

    if spec.tag != locked.tag {
        return false;
    }
    if let Some(expected_rev) = &spec.rev
        && expected_rev != &locked.rev
    {
        return false;
    }

    let expected_bin = spec.bin.as_deref().unwrap_or(name);
    locked.path.as_deref() == Some(expected_bin)
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
            LockedPackageKind::Plugin => return Ok(true),
            LockedPackageKind::Other => return Ok(true),
        }
    }

    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::ffi::OsString;
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

    fn plugin_test_name(base: &str) -> OsString {
        if cfg!(windows) {
            OsString::from(format!("{base}.exe"))
        } else {
            OsString::from(base)
        }
    }

    #[test]
    fn writes_activate_overlay() {
        let project_dir = make_temp_dir("activate_overlay");
        let nu_env_dir = project_dir.join(".nu-env");

        write_activate_overlay(&nu_env_dir, &project_dir).unwrap();

        let activate = std::fs::read_to_string(nu_env_dir.join("activate.nu")).unwrap();
        assert!(activate.contains("export-env {"));
        assert!(activate.contains("source-env"));
        assert!(activate.contains("export alias nu = ^"));
        assert!(activate.contains("^"));
        assert!(activate.contains(".nu-env/bin/nu"));
        assert!(activate.contains("--config"));
        assert!(activate.contains(".nu-env/config.nu"));
        assert!(activate.contains("use std [ 'path add' ]"));
        assert!(activate.contains("path add"));
        assert!(activate.contains("export alias deactivate = overlay hide activate"));

        let _ = std::fs::remove_dir_all(project_dir);
    }

    #[test]
    fn writes_config_nu() {
        let nu_env_dir = make_temp_dir("config_nu");
        let modules_dir = nu_env_dir.join("modules");
        std::fs::create_dir_all(&modules_dir).unwrap();

        write_config_nu(&nu_env_dir, &modules_dir).unwrap();

        let config_nu = std::fs::read_to_string(nu_env_dir.join("config.nu")).unwrap();
        assert!(config_nu.contains("export const NU_LIB_DIRS"));
        assert!(config_nu.contains(&modules_dir.display().to_string()));
        assert!(config_nu.contains("$env.NU_LIB_DIRS"));
        assert!(config_nu.contains("use std [ 'path add' ]"));
        assert!(config_nu.contains("path add"));
        assert!(config_nu.contains("export alias nu = ^"));
        assert!(config_nu.contains("--config"));

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
    fn local_lockfile_staleness_detects_changed_module_ref() {
        let manifest = Manifest::from_str(
            r#"[package]
name = "demo"
version = "0.1.0"

[dependencies.modules]
nu-salesforce = { git = "https://github.com/freepicheep/nu-salesforce", tag = "v0.4.0" }
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
"#,
        )
        .unwrap();

        assert!(local_lockfile_is_stale(&manifest, &lockfile));
    }

    #[test]
    fn local_lockfile_staleness_detects_changed_plugin_bin() {
        let manifest = Manifest::from_str(
            r#"[package]
name = "demo"
version = "0.1.0"

[dependencies.plugins]
nu_plugin_inc = { git = "https://github.com/nushell/nu_plugin_inc", tag = "v0.91.0", bin = "nu_plugin_inc_v2" }
"#,
        )
        .unwrap();
        let lockfile = Lockfile::from_str(
            r#"version = 1

[[package]]
name = "nu_plugin_inc"
kind = "plugin"
git = "https://github.com/nushell/nu_plugin_inc"
tag = "v0.91.0"
rev = "dddddddddddddddddddddddddddddddddddddddd"
path = "nu_plugin_inc"
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

        install(&project_dir, true, false, false).unwrap();

        let nu_env = project_dir.join(".nu-env");
        let activate = std::fs::read_to_string(nu_env.join("activate.nu")).unwrap();
        assert!(activate.contains("export alias nu = ^"));
        assert!(activate.contains("source-env"));
        assert!(activate.contains("export alias deactivate = overlay hide activate"));

        let config_nu = std::fs::read_to_string(nu_env.join("config.nu")).unwrap();
        assert!(config_nu.contains("export const NU_LIB_DIRS"));
        assert!(config_nu.contains(".nu-env/modules"));

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
            install_mode: InstallMode::Copy,
            security: crate::config::SecurityConfig::default(),
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
            install_mode: InstallMode::Copy,
            security: crate::config::SecurityConfig::default(),
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

        create_nu_symlink(&nu_env_dir, None).unwrap();

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

        create_nu_symlink(&nu_env_dir, None).unwrap();

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
    fn local_lockfile_staleness_detects_missing_plugin_entry() {
        let manifest = Manifest::from_str(
            r#"
[package]
name = "demo"
version = "0.1.0"

[dependencies.modules]
nu-utils = { git = "https://github.com/example/nu-utils", tag = "v1.0.0" }

[dependencies.plugins]
nu_plugin_inc = { git = "https://github.com/nushell/nu_plugin_inc", tag = "v0.91.0", bin = "nu_plugin_inc" }
"#,
        )
        .unwrap();

        let lockfile = Lockfile {
            version: 1,
            packages: vec![LockedPackage {
                name: "nu-utils".to_string(),
                kind: LockedPackageKind::Module,
                git: "https://github.com/example/nu-utils".to_string(),
                tag: Some("v1.0.0".to_string()),
                rev: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                path: None,
                sha256: "aaa".to_string(),
                asset_sha256: None,
                asset_url: None,
            }],
        };

        assert!(local_lockfile_is_stale(&manifest, &lockfile));
    }

    #[test]
    fn select_plugin_release_asset_prefers_matching_tag_and_platform() {
        let binary = plugin_binary_filename("nu_plugin_inc");
        let releases = vec![
            GitHubRelease {
                tag_name: "v0.90.0".to_string(),
                draft: false,
                prerelease: false,
                assets: vec![GitHubReleaseAsset {
                    name: "nu_plugin_inc-x86_64-unknown-linux-gnu.tar.gz".to_string(),
                    browser_download_url: "https://example.com/old".to_string(),
                }],
            },
            GitHubRelease {
                tag_name: "v0.91.0".to_string(),
                draft: false,
                prerelease: false,
                assets: vec![GitHubReleaseAsset {
                    name: format!(
                        "nu_plugin_inc-{}-{}.tar.gz",
                        std::env::consts::ARCH,
                        std::env::consts::OS
                    ),
                    browser_download_url: "https://example.com/new".to_string(),
                }],
            },
        ];

        let selected = select_plugin_release_asset(&releases, Some("v0.91.0"), &binary);
        assert!(selected.is_some());
    }

    #[test]
    fn link_plugin_into_env_creates_link() {
        let root = make_temp_dir("plugin_link");
        let store_bin = root.join("store").join("nu_plugin_inc");
        let env_bin = root.join(".nu-env").join("bin");
        std::fs::create_dir_all(store_bin.parent().unwrap()).unwrap();
        std::fs::write(&store_bin, "plugin-binary").unwrap();

        link_plugin_into_env(&store_bin, &env_bin, "nu_plugin_inc").unwrap();

        let linked = env_bin.join("nu_plugin_inc");
        assert!(linked.exists());
        #[cfg(unix)]
        assert!(linked.symlink_metadata().unwrap().file_type().is_symlink());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn find_extracted_nu_plugins_finds_only_core_plugins() {
        let root = make_temp_dir("nu_release_plugins");
        let extracted = root.join("extract");
        let nested = extracted.join("nu-0.110.0");
        std::fs::create_dir_all(&nested).unwrap();

        std::fs::write(nested.join("nu"), "nu").unwrap();
        std::fs::write(nested.join("README.txt"), "readme").unwrap();
        let formats = plugin_test_name("nu_plugin_formats");
        let query = plugin_test_name("nu_plugin_query");
        std::fs::write(nested.join(&formats), "formats").unwrap();
        std::fs::write(nested.join(&query), "query").unwrap();

        let plugins = find_extracted_nu_plugins(&extracted);
        let names: Vec<String> = plugins
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        assert_eq!(
            names,
            vec![
                formats.to_string_lossy().into_owned(),
                query.to_string_lossy().into_owned()
            ]
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn install_extracted_nu_plugins_copies_into_shared_plugins_store() {
        let root = make_temp_dir("nu_release_plugin_install");
        let extracted = root.join("extract");
        std::fs::create_dir_all(&extracted).unwrap();
        let formats = plugin_test_name("nu_plugin_formats");
        let query = plugin_test_name("nu_plugin_query");
        std::fs::write(extracted.join(&formats), "formats").unwrap();
        std::fs::write(extracted.join(&query), "query").unwrap();

        let plugins_root = root.join("plugins");
        let version = Version::parse("0.110.0").unwrap();
        let count = install_extracted_nu_plugins(&extracted, &plugins_root, &version).unwrap();
        assert_eq!(count, 2);
        assert!(
            plugins_root
                .join("nu_plugin_formats")
                .join("nu-0.110.0")
                .join("bin")
                .join(formats)
                .is_file()
        );
        assert!(
            plugins_root
                .join("nu_plugin_query")
                .join("nu-0.110.0")
                .join("bin")
                .join(query)
                .is_file()
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn find_core_plugin_binary_prefers_same_dir_as_nu() {
        let root = make_temp_dir("core_plugin_same_dir");
        let nu_dir = root.join("usr").join("local").join("bin");
        std::fs::create_dir_all(&nu_dir).unwrap();
        let nu_path = nu_dir.join("nu");
        std::fs::write(&nu_path, "nu").unwrap();
        let plugin_name = plugin_test_name("nu_plugin_query");
        let plugin_path = nu_dir.join(&plugin_name);
        std::fs::write(&plugin_path, "query").unwrap();

        let found =
            find_core_plugin_binary_for_nu(&nu_path, &plugin_name.to_string_lossy()).unwrap();
        assert_eq!(found, plugin_path);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn find_core_plugin_binary_checks_quiver_version_plugins_dir() {
        let root = make_temp_dir("core_plugin_quiver_store");
        let version_bin = root.join("nu_versions").join("0.110.0").join("bin");
        std::fs::create_dir_all(&version_bin).unwrap();
        let nu_path = version_bin.join("nu");
        std::fs::write(&nu_path, "nu").unwrap();
        let plugin_name = plugin_test_name("nu_plugin_polars");
        let plugins_dir = root.join("nu_versions").join("0.110.0").join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        let plugin_path = plugins_dir.join(&plugin_name);
        std::fs::write(&plugin_path, "polars").unwrap();

        let found =
            find_core_plugin_binary_for_nu(&nu_path, &plugin_name.to_string_lossy()).unwrap();
        assert_eq!(found, plugin_path);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn nu_string_literal_escapes_paths() {
        let literal = nu_string_literal(Path::new(r#"/tmp/dir "with quote"/nu"#));
        assert_eq!(literal, r#""/tmp/dir \"with quote\"/nu""#);
    }

    #[test]
    fn plugin_registration_commands_are_sorted_and_deduped() {
        let commands = plugin_registration_commands(&[
            ResolvedPlugin {
                name: "z_plugin".to_string(),
                git: "nu-core".to_string(),
                tag: None,
                rev: "nu-core".to_string(),
                bin: Some("nu_plugin_zeta".to_string()),
            },
            ResolvedPlugin {
                name: "a_plugin".to_string(),
                git: "nu-core".to_string(),
                tag: None,
                rev: "nu-core".to_string(),
                bin: None,
            },
            ResolvedPlugin {
                name: "duplicate".to_string(),
                git: "nu-core".to_string(),
                tag: None,
                rev: "nu-core".to_string(),
                bin: Some("nu_plugin_zeta".to_string()),
            },
        ]);

        assert_eq!(
            commands,
            vec![
                ("a_plugin".to_string(), "a_plugin".to_string()),
                ("nu_plugin_zeta".to_string(), "zeta".to_string())
            ]
        );
    }

    #[test]
    fn plugin_use_name_strips_prefix_and_windows_suffix() {
        assert_eq!(plugin_use_name("nu_plugin_inc"), "inc");
        assert_eq!(plugin_use_name("nu_plugin_query.exe"), "query");
        assert_eq!(plugin_use_name("custom_plugin"), "custom_plugin");
    }

    #[test]
    fn parse_yes_no_accepts_expected_answers() {
        assert_eq!(parse_yes_no("y"), Some(true));
        assert_eq!(parse_yes_no("yes"), Some(true));
        assert_eq!(parse_yes_no("Y"), Some(true));
        assert_eq!(parse_yes_no(" n "), Some(false));
        assert_eq!(parse_yes_no("no"), Some(false));
        assert_eq!(parse_yes_no(""), Some(false));
    }

    #[test]
    fn parse_yes_no_rejects_invalid_answers() {
        assert_eq!(parse_yes_no("maybe"), None);
        assert_eq!(parse_yes_no("1"), None);
    }

    #[test]
    fn parse_expected_sha256_supports_common_formats() {
        let asset = "nu-x86_64-unknown-linux-gnu.tar.gz";
        let hash = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let checksum_text =
            format!("{hash}  {asset}\n{hash} *{asset}\nSHA256 ({asset}) = {hash}\n");
        let parsed = parse_expected_sha256(&checksum_text, asset).unwrap();
        assert_eq!(parsed, hash);
    }

    #[test]
    fn parse_expected_sha256_rejects_malformed_digest() {
        let asset = "nu-x86_64-unknown-linux-gnu.tar.gz";
        let err = parse_expected_sha256("abc123  nu-x86_64-unknown-linux-gnu.tar.gz", asset)
            .unwrap_err()
            .to_string();
        assert!(err.contains("checksum parse failure"));
        assert!(err.contains(asset));
    }

    #[test]
    fn verify_downloaded_asset_detects_match_and_mismatch() {
        let root = make_temp_dir("asset_verify");
        let asset_path = root.join("sample.tar.gz");
        std::fs::write(&asset_path, b"quiver-security").unwrap();

        let digest = sha256_file(&asset_path).unwrap();
        assert!(verify_downloaded_asset(&asset_path, "sample.tar.gz", &digest).is_ok());

        let mismatch = verify_downloaded_asset(
            &asset_path,
            "sample.tar.gz",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        )
        .unwrap_err()
        .to_string();
        assert!(mismatch.contains("checksum mismatch"));
        assert!(mismatch.contains("sample.tar.gz"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn frozen_policy_forces_signed_assets_and_disables_fallback() {
        let policy = security_policy_for(false, true, true, false);
        assert!(policy.require_signed_assets);
        assert!(!policy.allow_unsigned);
        assert!(policy.no_build_fallback);
    }

    #[test]
    fn frozen_checksum_verification_requires_matching_sha() {
        let lock = Lockfile {
            version: 1,
            packages: vec![LockedPackage {
                name: "nu-utils".to_string(),
                kind: LockedPackageKind::Module,
                git: "https://github.com/example/nu-utils".to_string(),
                tag: Some("v1.0.0".to_string()),
                rev: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                path: None,
                sha256: "deadbeef".to_string(),
                asset_sha256: None,
                asset_url: None,
            }],
        };

        assert!(
            verify_frozen_checksum(
                Some(&lock),
                "nu-utils",
                LockedPackageKind::Module,
                "deadbeef"
            )
            .is_ok()
        );
        assert!(
            verify_frozen_checksum(
                Some(&lock),
                "nu-utils",
                LockedPackageKind::Module,
                "cafebabe"
            )
            .is_err()
        );
    }
}
