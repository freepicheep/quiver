mod checksum;
mod cli;
mod config;
mod error;
mod git;
mod installer;
mod lockfile;
mod manifest;
mod nu;
mod resolver;
mod safety;
mod ui;

use std::io::{self, Write};
use std::path::Path;
use std::process::Command;
#[cfg(test)]
use std::sync::{LazyLock, Mutex};

use cli::Commands;
use config::GlobalConfig;
use error::Result;
use manifest::{DependencySpec, Manifest, Package, PluginDependencySpec};
use safety::{validate_binary_name, validate_dependency_name};

#[cfg(test)]
pub(crate) static TEST_ENV_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn main() {
    let cli = cli::parse();

    if let Err(e) = run(cli.command) {
        ui::error(format!("{e}"));
        std::process::exit(1);
    }
}

fn run(command: Commands) -> Result<()> {
    let cwd = std::env::current_dir()?;

    match command {
        Commands::Init {
            name,
            version,
            nu_version,
            description,
        } => cmd_init(&cwd, name, version, nu_version, description),
        Commands::Install {
            global,
            frozen,
            allow_unsigned,
            no_build_fallback,
        } => {
            if global {
                cmd_install_global(frozen, allow_unsigned, no_build_fallback)
            } else {
                cmd_install(&cwd, frozen, allow_unsigned, no_build_fallback)
            }
        }
        Commands::Update => cmd_update(&cwd),
        Commands::Add {
            global,
            url,
            tag,
            rev,
            branch,
        } => {
            if global {
                cmd_add_global(url, tag, rev, branch)
            } else {
                cmd_add(&cwd, url, tag, rev, branch)
            }
        }
        Commands::AddPlugin {
            url,
            tag,
            rev,
            branch,
            bin,
        } => cmd_add_plugin(&cwd, url, tag, rev, branch, bin),
        Commands::Remove { global, name } => {
            if global {
                cmd_remove_global(name)
            } else {
                cmd_remove(&cwd, name)
            }
        }
        Commands::List => cmd_list(&cwd),
        Commands::Version => cmd_version(),
        Commands::Hook => cmd_hook(),
        Commands::Lsp { editor } => cmd_lsp(&cwd, editor),
        Commands::Run { command } => cmd_run(&cwd, command),
    }
}

fn cmd_init(
    dir: &Path,
    name: Option<String>,
    version: String,
    nu_version: Option<String>,
    description: Option<String>,
) -> Result<()> {
    let nupackage_toml = dir.join("nupackage.toml");
    if nupackage_toml.exists() {
        return Err(error::QuiverError::Manifest(
            "nupackage.toml already exists in this directory".to_string(),
        ));
    }

    // Default name to directory name
    let pkg_name = name.unwrap_or_else(|| {
        dir.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("my-module")
            .to_string()
    });
    if let Some(ref requested_nu_version) = nu_version {
        nu::parse_nu_version_requirement(requested_nu_version).map_err(|err| {
            error::QuiverError::Manifest(format!(
                "package nu-version '{requested_nu_version}' is invalid: {err}"
            ))
        })?;
    }

    let manifest = Manifest {
        package: Package {
            name: pkg_name.clone(),
            version,
            description: Some(description.unwrap_or_default()),
            license: Some(String::new()),
            authors: detect_git_author_name().map(|author| vec![author]),
            nu_version: nu_version.or_else(detect_nu_version),
        },
        dependencies: Default::default(),
    };

    let content = manifest.to_toml_string()?;
    std::fs::write(&nupackage_toml, content)?;
    eprintln!("Created nupackage.toml for '{pkg_name}'");

    // Create <current-dir-name>/mod.nu if it doesn't exist.
    let module_dir_name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(pkg_name.as_str());
    let module_dir = dir.join(module_dir_name);
    std::fs::create_dir_all(&module_dir)?;

    let mod_nu = module_dir.join("mod.nu");
    if !mod_nu.exists() {
        std::fs::write(
            &mod_nu,
            r#"# Module entry point
# Export your commands here with: export use <submodule>
# Use installed modules with: use module-name *
# Set up your lsp with `qv lsp`
"#,
        )?;
        eprintln!("Created {}", mod_nu.display());
    }

    // Generate .nu-env/ with activate.nu, config.nu, and bin/nu symlink
    let nu_env_dir = dir.join(".nu-env");
    let modules_dir = nu_env_dir.join("modules");
    std::fs::create_dir_all(&modules_dir)?;
    installer::write_config_nu(&nu_env_dir, &modules_dir)?;
    installer::create_nu_symlink(&nu_env_dir, manifest.package.nu_version.as_deref())?;
    installer::write_activate_overlay(&nu_env_dir, dir)?;
    ensure_gitignore_ignores_nu_env(dir)?;

    Ok(())
}

fn ensure_gitignore_ignores_nu_env(dir: &Path) -> Result<()> {
    let gitignore_path = dir.join(".gitignore");
    let ignore_entry = ".nu-env/";

    if !gitignore_path.exists() {
        std::fs::write(&gitignore_path, format!("{ignore_entry}\n"))?;
        eprintln!("Created .gitignore");
        return Ok(());
    }

    let existing = std::fs::read_to_string(&gitignore_path)?;
    let has_nu_env_entry = existing
        .lines()
        .any(|line| matches!(line.trim(), ".nu-env/" | ".nu-env"));
    if has_nu_env_entry {
        return Ok(());
    }

    let mut updated = existing;
    if !updated.ends_with('\n') && !updated.is_empty() {
        updated.push('\n');
    }
    updated.push_str(ignore_entry);
    updated.push('\n');
    std::fs::write(&gitignore_path, updated)?;
    eprintln!("Updated .gitignore with .nu-env/");

    Ok(())
}

fn cmd_install(
    dir: &Path,
    frozen: bool,
    allow_unsigned: bool,
    no_build_fallback: bool,
) -> Result<()> {
    installer::install(dir, frozen, allow_unsigned, no_build_fallback)
}

fn cmd_install_global(frozen: bool, allow_unsigned: bool, no_build_fallback: bool) -> Result<()> {
    installer::install_global(frozen, allow_unsigned, no_build_fallback)
}

fn cmd_update(dir: &Path) -> Result<()> {
    installer::update(dir)
}

fn cmd_add(
    dir: &Path,
    url: String,
    tag: Option<String>,
    rev: Option<String>,
    branch: Option<String>,
) -> Result<()> {
    // Load existing manifest (or error if none)
    let mut manifest = Manifest::from_dir(dir)?;
    let provider_base = if is_git_url(url.trim()) {
        None
    } else {
        let config = GlobalConfig::load_or_default()?;
        Some(config.default_git_provider_base_url()?)
    };
    let url = normalize_dependency_source(&url, provider_base.as_deref())?;

    // Derive package name from URL
    let pkg_name = git::repo_name_from_url(&url).ok_or_else(|| {
        error::QuiverError::Other(format!("could not determine package name from URL: {url}"))
    })?;
    validate_dependency_name(&pkg_name, "module dependency")?;

    // Check if already added
    if manifest.dependencies.modules.contains_key(&pkg_name) {
        return Err(error::QuiverError::Manifest(format!(
            "dependency '{pkg_name}' already exists in nupackage.toml"
        )));
    }

    // If no ref spec given, auto-detect: try latest tag, fall back to default branch
    let dep_spec = auto_detect_dep_spec(&url, tag, rev, branch)?;

    dep_spec.validate(&pkg_name)?;

    // Add to manifest and write back
    manifest
        .dependencies
        .modules
        .insert(pkg_name.clone(), dep_spec);
    let content = manifest.to_toml_string()?;
    std::fs::write(dir.join("nupackage.toml"), content)?;

    ui::success(format!("Added module '{pkg_name}' to nupackage.toml"));

    // Run install
    installer::install_with_options(dir, false, false, false, false)
}

fn cmd_add_plugin(
    dir: &Path,
    url: String,
    tag: Option<String>,
    rev: Option<String>,
    branch: Option<String>,
    bin: Option<String>,
) -> Result<()> {
    let mut manifest = Manifest::from_dir(dir)?;

    if !is_git_url(url.trim())
        && let Some(core_plugin_name) = resolve_core_plugin_name(&url)
    {
        if manifest
            .dependencies
            .plugins
            .contains_key(&core_plugin_name)
        {
            return Err(error::QuiverError::Manifest(format!(
                "plugin dependency '{core_plugin_name}' already exists in nupackage.toml"
            )));
        }

        let dep_spec = PluginDependencySpec {
            source: Some("nu-core".to_string()),
            git: String::new(),
            tag: None,
            rev: None,
            branch: None,
            bin: Some(core_plugin_name.clone()),
        };
        dep_spec.validate(&core_plugin_name)?;
        manifest
            .dependencies
            .plugins
            .insert(core_plugin_name.clone(), dep_spec);
        let content = manifest.to_toml_string()?;
        std::fs::write(dir.join("nupackage.toml"), content)?;
        ui::success(format!(
            "Added core plugin '{core_plugin_name}' to nupackage.toml"
        ));
        return installer::install_with_options(dir, false, false, false, true);
    }

    let provider_base = if is_git_url(url.trim()) {
        None
    } else {
        let config = GlobalConfig::load_or_default()?;
        Some(config.default_git_provider_base_url()?)
    };
    let url = normalize_dependency_source(&url, provider_base.as_deref())?;

    let pkg_name = git::repo_name_from_url(&url).ok_or_else(|| {
        error::QuiverError::Other(format!("could not determine package name from URL: {url}"))
    })?;
    validate_dependency_name(&pkg_name, "plugin dependency")?;
    if let Some(ref bin_name) = bin {
        validate_binary_name(bin_name, "plugin dependency bin")?;
    }

    if manifest.dependencies.plugins.contains_key(&pkg_name) {
        return Err(error::QuiverError::Manifest(format!(
            "plugin dependency '{pkg_name}' already exists in nupackage.toml"
        )));
    }

    let dep_spec = if tag.is_none() && rev.is_none() && branch.is_none() {
        ui::info(format!(
            "{} plugin source {} to detect version",
            ui::keyword("Fetching"),
            url
        ));
        let repo_path = git::clone_or_fetch(&url)?;
        if let Some(latest) = git::latest_tag(&repo_path)? {
            ui::info(format!("{} latest tag {}", ui::keyword("Found"), latest));
            PluginDependencySpec {
                source: None,
                git: url.to_string(),
                tag: Some(latest),
                rev: None,
                branch: None,
                bin,
            }
        } else {
            let default_br = git::default_branch(&repo_path)?;
            ui::warn(format!(
                "{} tags found; using branch {}",
                ui::keyword("No"),
                default_br
            ));
            PluginDependencySpec {
                source: None,
                git: url.to_string(),
                tag: None,
                rev: None,
                branch: Some(default_br),
                bin,
            }
        }
    } else {
        PluginDependencySpec {
            source: None,
            git: url.to_string(),
            tag,
            rev,
            branch,
            bin,
        }
    };

    dep_spec.validate(&pkg_name)?;
    manifest
        .dependencies
        .plugins
        .insert(pkg_name.clone(), dep_spec);
    let content = manifest.to_toml_string()?;
    std::fs::write(dir.join("nupackage.toml"), content)?;
    ui::success(format!("Added plugin '{pkg_name}' to nupackage.toml"));

    installer::install_with_options(dir, false, false, false, true)
}

fn cmd_add_global(
    url: String,
    tag: Option<String>,
    rev: Option<String>,
    branch: Option<String>,
) -> Result<()> {
    let mut config = GlobalConfig::load()?;
    let provider_base = if is_git_url(url.trim()) {
        None
    } else {
        Some(config.default_git_provider_base_url()?)
    };
    let url = normalize_dependency_source(&url, provider_base.as_deref())?;

    // Derive package name from URL
    let pkg_name = git::repo_name_from_url(&url).ok_or_else(|| {
        error::QuiverError::Other(format!("could not determine package name from URL: {url}"))
    })?;
    validate_dependency_name(&pkg_name, "module dependency")?;

    // Check if already added
    if config.dependencies.contains_key(&pkg_name) {
        return Err(error::QuiverError::Config(format!(
            "dependency '{pkg_name}' already exists in global config"
        )));
    }

    let dep_spec = auto_detect_dep_spec(&url, tag, rev, branch)?;

    dep_spec.validate(&pkg_name)?;

    // Add to global config and save
    config.dependencies.insert(pkg_name.clone(), dep_spec);
    config.save()?;

    eprintln!("Added '{pkg_name}' to global config");

    // Run global install
    installer::install_global(false, false, false)
}

fn cmd_remove(dir: &Path, name: String) -> Result<()> {
    validate_dependency_name(&name, "dependency")?;
    // Load existing manifest
    let mut manifest = Manifest::from_dir(dir)?;

    // Try removing as a module first
    if manifest.dependencies.modules.remove(&name).is_some() {
        // Write updated manifest
        let content = manifest.to_toml_string()?;
        std::fs::write(dir.join("nupackage.toml"), content)?;
        eprintln!("Removed module '{name}' from nupackage.toml");

        // Remove from .nu-env/modules/
        let module_dir = dir.join(".nu-env").join("modules").join(&name);
        if module_dir.exists() {
            std::fs::remove_dir_all(&module_dir)?;
            eprintln!("Removed .nu-env/modules/{name}/");
        }
        for removed in remove_module_dist_info_dirs(&dir.join(".nu-env").join("modules"), &name)? {
            eprintln!("Removed .nu-env/modules/{removed}/");
        }

        // Update lockfile: remove the module package entry
        let lock_path = dir.join("quiver.lock");
        if lock_path.exists() {
            let mut lockfile = lockfile::Lockfile::from_path(&lock_path)?;
            lockfile
                .packages
                .retain(|p| !(p.name == name && p.kind == lockfile::LockedPackageKind::Module));
            lockfile.write_to(&lock_path)?;
            eprintln!("Updated quiver.lock");
        }
    } else if let Some(plugin_spec) = manifest.dependencies.plugins.remove(&name) {
        // Removing a plugin dependency
        let content = manifest.to_toml_string()?;
        std::fs::write(dir.join("nupackage.toml"), content)?;
        eprintln!("Removed plugin '{name}' from nupackage.toml");

        // Remove plugin binary symlink from .nu-env/bin/
        let bin_name = plugin_spec.bin.as_deref().unwrap_or(&name);
        validate_binary_name(bin_name, "plugin dependency bin")?;
        let binary_filename = if cfg!(windows) && !bin_name.ends_with(".exe") {
            format!("{bin_name}.exe")
        } else {
            bin_name.to_string()
        };
        let plugin_link = dir.join(".nu-env").join("bin").join(&binary_filename);
        if plugin_link.exists() || plugin_link.symlink_metadata().is_ok() {
            std::fs::remove_file(&plugin_link)?;
            eprintln!("Removed .nu-env/bin/{binary_filename}");
        }

        // Update lockfile: remove the plugin package entry
        let lock_path = dir.join("quiver.lock");
        if lock_path.exists() {
            let mut lockfile = lockfile::Lockfile::from_path(&lock_path)?;
            lockfile
                .packages
                .retain(|p| !(p.name == name && p.kind == lockfile::LockedPackageKind::Plugin));
            lockfile.write_to(&lock_path)?;
            eprintln!("Updated quiver.lock");
        }
    } else {
        return Err(error::QuiverError::Manifest(format!(
            "dependency '{name}' not found in nupackage.toml"
        )));
    }

    // Regenerate activate.nu from the updated manifest and lockfile state.
    eprintln!("Regenerating activate.nu...");
    installer::install_with_options(dir, false, false, false, false)?;

    Ok(())
}

fn cmd_remove_global(name: String) -> Result<()> {
    validate_dependency_name(&name, "dependency")?;
    let mut config = GlobalConfig::load()?;

    // Check the dep exists
    if config.dependencies.remove(&name).is_none() {
        return Err(error::QuiverError::Config(format!(
            "dependency '{name}' not found in global config"
        )));
    }

    // Save updated config
    config.save()?;
    eprintln!("Removed '{name}' from global config");

    // Remove from global modules dir
    let modules_dir = config.modules_dir()?;
    let module_dir = modules_dir.join(&name);
    if module_dir.exists() {
        std::fs::remove_dir_all(&module_dir)?;
        eprintln!("Removed {}/", module_dir.display());
    }
    for removed in remove_module_dist_info_dirs(&modules_dir, &name)? {
        eprintln!("Removed {}/", modules_dir.join(&removed).display());
    }

    // Update global lockfile
    let lock_path = config::global_lock_path()?;
    if lock_path.exists() {
        let mut lockfile = lockfile::Lockfile::from_path(&lock_path)?;
        lockfile
            .packages
            .retain(|p| !(p.name == name && p.kind == lockfile::LockedPackageKind::Module));
        lockfile.write_to(&lock_path)?;
        eprintln!("Updated global lockfile");
    }

    // Regenerate the activate.nu overlay with remaining global packages
    eprintln!("Regenerating global activate.nu...");
    installer::install_global(false, false, false)?;

    Ok(())
}

fn cmd_run(cwd: &Path, command: Vec<String>) -> Result<()> {
    if command.is_empty() {
        return Err(error::QuiverError::Other(
            "missing command to run".to_string(),
        ));
    }

    if !cwd.join("nupackage.toml").exists() {
        return Err(error::QuiverError::NoManifest(cwd.to_path_buf()));
    }

    let manifest = Manifest::from_dir(cwd)?;
    let config_path = cwd.join(".nu-env").join("config.nu");
    let plugin_config_path = cwd.join(".nu-env").join("plugins.msgpackz");
    let needs_install = !config_path.exists()
        || (!manifest.dependencies.plugins.is_empty() && !plugin_config_path.exists());

    if needs_install {
        eprintln!("Project environment is incomplete; running install first...");
        installer::install_with_options(cwd, false, false, false, false)?;
    }

    if !config_path.exists() {
        return Err(error::QuiverError::Other(
            "failed to create .nu-env/config.nu".to_string(),
        ));
    }

    let (program, args) = resolve_run_invocation(&command, &config_path, &plugin_config_path, cwd);
    let status = Command::new(&program)
        .args(&args)
        .current_dir(cwd)
        .status()?;

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }

    Ok(())
}

fn resolve_run_invocation(
    command: &[String],
    config_path: &Path,
    plugin_config_path: &Path,
    cwd: &Path,
) -> (String, Vec<String>) {
    let executable = command[0].clone();
    let mut args = command[1..].to_vec();
    let config = config_path.to_string_lossy().to_string();
    let plugin_config = plugin_config_path.to_string_lossy().to_string();

    if executable == "nu" {
        if !args.iter().any(|arg| arg == "--plugin-config") {
            args.insert(0, plugin_config);
            args.insert(0, "--plugin-config".to_string());
        }
        if !args.iter().any(|arg| arg == "--config") {
            args.insert(0, config);
            args.insert(0, "--config".to_string());
        }
        return (executable, args);
    }

    if is_nushell_script_command(&executable, cwd) {
        let mut nu_args = vec![
            "--config".to_string(),
            config,
            "--plugin-config".to_string(),
            plugin_config,
            executable,
        ];
        nu_args.extend(args);
        return ("nu".to_string(), nu_args);
    }

    (executable, args)
}

fn is_nushell_script_command(executable: &str, cwd: &Path) -> bool {
    let command_path = Path::new(executable);
    if command_path.extension().and_then(|ext| ext.to_str()) != Some("nu") {
        return false;
    }

    if command_path.is_absolute() {
        return true;
    }

    cwd.join(command_path)
        .extension()
        .and_then(|ext| ext.to_str())
        == Some("nu")
}

fn cmd_hook() -> Result<()> {
    let hook_script = r#"# quiver auto-activate hook
# Add this to your Nushell environment by running:
#   mkdir ($nu.default-config-dir | path join "vendor" "autoload")
#   qv hook | save -f ($nu.default-config-dir | path join "vendor" "autoload" "quiver_hook.nu")
# Once saved, it will be automatically sourced when you start Nushell.
# You can add the above to your config.nu if you want any updates to the hook, but that may slow start time.

$env.config.hooks.env_change.PWD = (
    $env.config.hooks.env_change.PWD? | default [] | append {|before after|
        let before = ($before | default "")
        let after = ($after | default "")

        # Remove previous directory's modules if it was a quiver project
        if ($before | is-not-empty) and ($before | path join "nupackage.toml" | path exists) {
            let old_modules = ($before | path join ".nu-env" "modules")
            $env.NU_LIB_DIRS = ($env.NU_LIB_DIRS | default [] | where {|it| $it != $old_modules })
        }
        # Add new directory's modules if it is a quiver project
        if ($after | is-not-empty) and ($after | path join "nupackage.toml" | path exists) {
            let new_modules = ($after | path join ".nu-env" "modules")
            if ($new_modules | path exists) and ($new_modules not-in ($env.NU_LIB_DIRS | default [])) {
                $env.NU_LIB_DIRS = ($env.NU_LIB_DIRS | default [] | append $new_modules)
            }
        }
    }
)"#;
    println!("{hook_script}");
    Ok(())
}

fn cmd_version() -> Result<()> {
    println!("quiver {}", env!("CARGO_PKG_VERSION"));
    Ok(())
}

fn cmd_lsp(cwd: &Path, editors: Vec<String>) -> Result<()> {
    let all_editors = ["helix", "zed"];

    let selected: Vec<String> = if editors.is_empty() {
        // Interactive picker
        pick_editors(&all_editors)?
    } else {
        // Validate provided editor names
        for e in &editors {
            let lower = e.to_lowercase();
            if !all_editors.contains(&lower.as_str()) {
                return Err(error::QuiverError::Other(format!(
                    "unknown editor '{}'; supported: {}",
                    e,
                    all_editors.join(", ")
                )));
            }
        }
        editors.iter().map(|e| e.to_lowercase()).collect()
    };

    if selected.is_empty() {
        eprintln!("No editors selected.");
        return Ok(());
    }

    for editor in &selected {
        match editor.as_str() {
            "helix" => write_helix_lsp_config(cwd)?,
            "zed" => write_zed_lsp_config(cwd)?,
            _ => {}
        }
    }

    Ok(())
}

/// Interactive checkbox picker rendered to stderr.
fn pick_editors(options: &[&str]) -> Result<Vec<String>> {
    use std::io::Read;

    let mut selected = vec![false; options.len()];
    let mut cursor = 0usize;

    // Save terminal state and enable raw mode
    let stdin = io::stdin();
    let mut stderr = io::stderr();

    // Set raw mode via stty
    let _ = Command::new("stty")
        .arg("raw")
        .arg("-echo")
        .stdin(std::process::Stdio::inherit())
        .status();

    let render = |selected: &[bool], cursor: usize, out: &mut io::Stderr| -> io::Result<()> {
        // Move to start and clear
        write!(out, "\r\x1b[J")?;
        write!(
            out,
            "Select editors to configure (space=toggle, j/k=move, enter=confirm):\r\n"
        )?;
        for (i, option) in options.iter().enumerate() {
            let check = if selected[i] { "x" } else { " " };
            let prefix = if i == cursor { "> " } else { "  " };
            write!(out, "{prefix}[{check}] {option}\r\n")?;
        }
        out.flush()
    };

    render(&selected, cursor, &mut stderr)?;

    let result = (|| -> Result<Vec<String>> {
        let mut bytes = stdin.lock().bytes();
        loop {
            let b = match bytes.next() {
                Some(Ok(b)) => b,
                _ => break,
            };

            match b {
                b'\n' | b'\r' => {
                    // Enter — confirm
                    break;
                }
                b' ' => {
                    selected[cursor] = !selected[cursor];
                }
                b'j' => {
                    if cursor + 1 < options.len() {
                        cursor += 1;
                    }
                }
                b'k' => {
                    if cursor > 0 {
                        cursor -= 1;
                    }
                }
                b'q' | 3 => {
                    // q or Ctrl-C — abort
                    let _ = Command::new("stty")
                        .arg("sane")
                        .stdin(std::process::Stdio::inherit())
                        .status();
                    write!(stderr, "\r\x1b[J")?;
                    stderr.flush()?;
                    std::process::exit(0);
                }
                27 => {
                    // Escape sequence (arrow keys)
                    let next = bytes.next();
                    if let Some(Ok(b'[')) = next {
                        if let Some(Ok(arrow)) = bytes.next() {
                            match arrow {
                                b'A' => {
                                    if cursor > 0 {
                                        cursor -= 1;
                                    }
                                } // Up
                                b'B' => {
                                    if cursor + 1 < options.len() {
                                        cursor += 1;
                                    }
                                } // Down
                                _ => {}
                            }
                        }
                    }
                }
                _ => {}
            }

            // Move cursor up to re-render
            let lines_to_move = options.len() + 1; // +1 for the header line
            write!(stderr, "\x1b[{}A", lines_to_move)?;
            render(&selected, cursor, &mut stderr)?;
        }

        Ok(options
            .iter()
            .zip(selected.iter())
            .filter_map(|(name, &sel)| if sel { Some(name.to_string()) } else { None })
            .collect())
    })();

    // Restore terminal
    let _ = Command::new("stty")
        .arg("sane")
        .stdin(std::process::Stdio::inherit())
        .status();
    write!(stderr, "\r\x1b[J")?;
    stderr.flush()?;

    result
}

fn write_helix_lsp_config(project_dir: &Path) -> Result<()> {
    let helix_dir = project_dir.join(".helix");
    std::fs::create_dir_all(&helix_dir)?;

    let config_path = helix_dir.join("languages.toml");
    let config = r#"[language-server.nu-lsp]
command = ".nu-env/bin/nu"
args = ["--config .nu-env/config.nu", "--plugin-config .nu-env/plugins.msgpackz", "--lsp"]
"#;

    std::fs::write(&config_path, config)?;
    eprintln!("Generated .helix/languages.toml");
    Ok(())
}

fn write_zed_lsp_config(project_dir: &Path) -> Result<()> {
    let zed_dir = project_dir.join(".zed");
    std::fs::create_dir_all(&zed_dir)?;

    let config_path = zed_dir.join("settings.json");
    let config = r#"{
  "lsp": {
    "nu": {
      "binary": {
        "path": ".nu-env/bin/nu",
        "arguments": ["--config .nu-env/config.nu", "--plugin-config .nu-env/plugins.msgpackz", "--lsp"]
      }
    }
  }
}
"#;

    std::fs::write(&config_path, config)?;
    eprintln!("Generated .zed/settings.json");
    Ok(())
}

fn cmd_list(cwd: &Path) -> Result<()> {
    if cwd.join("nupackage.toml").exists() {
        let modules_dir = cwd.join(".nu-env").join("modules");
        let bin_dir = cwd.join(".nu-env").join("bin");
        let modules = list_installed_module_names(&modules_dir)?;
        let plugins = list_installed_plugin_names(&bin_dir)?;

        if modules.is_empty() && plugins.is_empty() {
            eprintln!("No installed project dependencies found.");
            return Ok(());
        }

        if !modules.is_empty() {
            eprintln!("Installed project modules:");
            for module in &modules {
                eprintln!("  {module}");
            }
        }

        if !plugins.is_empty() {
            if !modules.is_empty() {
                eprintln!();
            }
            eprintln!("Installed project plugins:");
            for plugin in &plugins {
                eprintln!("  {plugin}");
            }
        }
    } else {
        let config = GlobalConfig::load_or_default()?;
        let modules_dir = config.modules_dir()?;
        let modules = list_installed_module_names(&modules_dir)?;

        if modules.is_empty() {
            eprintln!(
                "No installed global dependencies found in {}",
                modules_dir.display(),
            );
            return Ok(());
        }

        eprintln!("Installed global modules from {}", modules_dir.display());
        for module in modules {
            eprintln!("{module}");
        }
    }

    Ok(())
}

fn list_installed_module_names(modules_dir: &Path) -> Result<Vec<String>> {
    if !modules_dir.exists() {
        return Ok(Vec::new());
    }

    let mut modules = Vec::new();

    for entry in std::fs::read_dir(modules_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        if let Some(name) = entry.file_name().to_str() {
            if name.ends_with(".dist-info") {
                continue;
            }
            modules.push(name.to_string());
        }
    }

    modules.sort();
    Ok(modules)
}

fn remove_module_dist_info_dirs(modules_dir: &Path, module_name: &str) -> Result<Vec<String>> {
    if !modules_dir.exists() {
        return Ok(Vec::new());
    }

    let mut removed = Vec::new();
    let prefix = format!("{module_name}-");

    for entry in std::fs::read_dir(modules_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if !name.starts_with(&prefix) || !name.ends_with(".dist-info") {
            continue;
        }

        std::fs::remove_dir_all(&path)?;
        removed.push(name.to_string());
    }

    removed.sort();
    Ok(removed)
}

fn list_installed_plugin_names(bin_dir: &Path) -> Result<Vec<String>> {
    if !bin_dir.exists() {
        return Ok(Vec::new());
    }

    let mut plugins = Vec::new();

    for entry in std::fs::read_dir(bin_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };

        // Strip .exe on Windows for display; only include nu_plugin_* entries
        let base_name = name_str.strip_suffix(".exe").unwrap_or(name_str);
        if base_name.starts_with("nu_plugin_") {
            plugins.push(base_name.to_string());
        }
    }

    plugins.sort();
    Ok(plugins)
}

fn normalize_dependency_source(input: &str, provider_base_url: Option<&str>) -> Result<String> {
    let trimmed = input.trim();

    if trimmed.is_empty() {
        return Err(error::QuiverError::Other(
            "dependency source cannot be empty".to_string(),
        ));
    }

    if is_git_url(trimmed) {
        safety::validate_secure_git_source(trimmed, "dependency source")?;
        return Ok(trimmed.to_string());
    }

    if is_repo_shorthand(trimmed) {
        let provider_base = provider_base_url.ok_or_else(|| {
            error::QuiverError::Other(
                "a default git provider is required for owner/repo shorthand".to_string(),
            )
        })?;
        let expanded = format!("{provider_base}/{trimmed}");
        safety::validate_secure_git_source(&expanded, "dependency source")?;
        return Ok(expanded);
    }

    Err(error::QuiverError::Other(format!(
        "invalid dependency source '{input}'; expected a git URL or owner/repo shorthand"
    )))
}

fn resolve_core_plugin_name(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    let normalized = trimmed.to_ascii_lowercase().replace('-', "_");
    let alias = match normalized.as_str() {
        "custom_values" => "nu_plugin_custom_values",
        "example" => "nu_plugin_example",
        "formats" => "nu_plugin_formats",
        "gstat" => "nu_plugin_gstat",
        "inc" => "nu_plugin_inc",
        "polars" | "polors" => "nu_plugin_polars",
        "query" => "nu_plugin_query",
        "stress_internals" => "nu_plugin_stress_internals",
        _ => "",
    };

    if !alias.is_empty() {
        return Some(alias.to_string());
    }

    if normalized.starts_with("nu_plugin_") {
        return Some(normalized);
    }

    None
}

fn is_git_url(value: &str) -> bool {
    value.contains("://") || value.starts_with("git@")
}

fn is_repo_shorthand(value: &str) -> bool {
    let mut parts = value.split('/');
    let owner = parts.next().unwrap_or_default();
    let repo = parts.next().unwrap_or_default();

    parts.next().is_none()
        && !owner.is_empty()
        && !repo.is_empty()
        && !owner.chars().any(char::is_whitespace)
        && !repo.chars().any(char::is_whitespace)
}

/// Auto-detect the dependency spec from a URL, optionally with an explicit ref.
///
/// If no tag/rev/branch is given, tries the latest tag first, then falls back
/// to the default branch.
fn auto_detect_dep_spec(
    url: &str,
    tag: Option<String>,
    rev: Option<String>,
    branch: Option<String>,
) -> Result<DependencySpec> {
    if tag.is_none() && rev.is_none() && branch.is_none() {
        ui::info(format!(
            "{} dependency source {} to detect version",
            ui::keyword("Fetching"),
            url
        ));
        let repo_path = git::clone_or_fetch(url)?;

        if let Some(latest) = git::latest_tag(&repo_path)? {
            ui::info(format!("{} latest tag {}", ui::keyword("Found"), latest));
            Ok(DependencySpec {
                git: url.to_string(),
                tag: Some(latest),
                rev: None,
                branch: None,
            })
        } else {
            let default_br = git::default_branch(&repo_path)?;
            ui::warn(format!(
                "{} tags found; using branch {}",
                ui::keyword("No"),
                default_br
            ));
            Ok(DependencySpec {
                git: url.to_string(),
                tag: None,
                rev: None,
                branch: Some(default_br),
            })
        }
    } else {
        Ok(DependencySpec {
            git: url.to_string(),
            tag,
            rev,
            branch,
        })
    }
}

fn detect_nu_version() -> Option<String> {
    let output = Command::new("nu").arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    nu::extract_semver_from_text(&stdout).map(|version| version.to_string())
}

fn detect_git_author_name() -> Option<String> {
    let output = Command::new("git")
        .args(["config", "user.name"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let author = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if author.is_empty() {
        None
    } else {
        Some(author)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn config_with_provider(provider: &str) -> GlobalConfig {
        GlobalConfig {
            modules_dir: None,
            default_git_provider: provider.to_string(),
            install_mode: config::InstallMode::Copy,
            security: config::SecurityConfig::default(),
            dependencies: HashMap::new(),
        }
    }

    fn make_temp_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "quiver_main_test_{}_{}_{}",
            label,
            std::process::id(),
            unique
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    struct EnvVarGuard {
        key: &'static str,
        original: Option<OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &Path) -> Self {
            let lock = crate::TEST_ENV_MUTEX
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let original = std::env::var_os(key);
            // SAFETY: tests serialize environment mutation through TEST_ENV_MUTEX.
            unsafe {
                std::env::set_var(key, value);
            }
            Self {
                key,
                original,
                _lock: lock,
            }
        }

        fn set_os(key: &'static str, value: OsString) -> Self {
            let lock = crate::TEST_ENV_MUTEX
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let original = std::env::var_os(key);
            // SAFETY: tests serialize environment mutation through TEST_ENV_MUTEX.
            unsafe {
                std::env::set_var(key, &value);
            }
            Self {
                key,
                original,
                _lock: lock,
            }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            // SAFETY: tests serialize environment mutation through TEST_ENV_MUTEX.
            unsafe {
                if let Some(value) = &self.original {
                    std::env::set_var(self.key, value);
                } else {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

    fn seed_fake_nu_store(version: &str) -> (PathBuf, EnvVarGuard) {
        let store_root = make_temp_dir("installs_root");
        let guard = EnvVarGuard::set("QUIVER_INSTALLS_ROOT", &store_root);
        let version_dir = store_root.join("nu_versions").join(version).join("bin");
        std::fs::create_dir_all(&version_dir).unwrap();
        std::fs::write(version_dir.join("nu"), "nu").unwrap();
        (store_root, guard)
    }

    fn seed_fake_git_config_user_name(stdout: &str) -> (PathBuf, EnvVarGuard) {
        let bin_dir = make_temp_dir("fake_git_bin");
        #[cfg(windows)]
        let git_path = bin_dir.join("git.bat");
        #[cfg(not(windows))]
        let git_path = bin_dir.join("git");

        #[cfg(windows)]
        let script = format!(
            "@echo off\r\nif \"%1\"==\"config\" if \"%2\"==\"user.name\" (\r\n  echo {}\r\n  exit /b 0\r\n)\r\nexit /b 1\r\n",
            stdout
        );
        #[cfg(not(windows))]
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = \"config\" ] && [ \"$2\" = \"user.name\" ]; then\n  printf '%s\\n' '{}'\n  exit 0\nfi\nexit 1\n",
            stdout.replace('\'', "'\"'\"'")
        );

        std::fs::write(&git_path, script).unwrap();
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&git_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&git_path, perms).unwrap();
        }

        let guard = EnvVarGuard::set_os("PATH", OsString::from(bin_dir.as_os_str()));
        (bin_dir, guard)
    }

    #[test]
    fn normalize_dependency_source_passes_through_urls() {
        let https = normalize_dependency_source("https://example.com/team/repo", None).unwrap();
        assert_eq!(https, "https://example.com/team/repo");

        let ssh = normalize_dependency_source("git@github.com:user/repo.git", None).unwrap();
        assert_eq!(ssh, "git@github.com:user/repo.git");
    }

    #[test]
    fn normalize_dependency_source_rejects_insecure_http_url() {
        let err = normalize_dependency_source("http://example.com/team/repo", None).unwrap_err();
        assert!(err.to_string().contains("insecure HTTP"));
    }

    #[test]
    fn normalize_dependency_source_expands_repo_shorthand() {
        let config = config_with_provider("github");
        let provider = config.default_git_provider_base_url().unwrap();
        let expanded =
            normalize_dependency_source("freepicheep/nu-salesforce", Some(provider.as_str()))
                .unwrap();
        assert_eq!(expanded, "https://github.com/freepicheep/nu-salesforce");
    }

    #[test]
    fn normalize_dependency_source_uses_custom_provider() {
        let config = config_with_provider("gitlab");
        let provider = config.default_git_provider_base_url().unwrap();
        let expanded = normalize_dependency_source("group/repo", Some(provider.as_str())).unwrap();
        assert_eq!(expanded, "https://gitlab.com/group/repo");
    }

    #[test]
    fn normalize_dependency_source_rejects_invalid_input() {
        let err = normalize_dependency_source("just-a-repo", None).unwrap_err();
        assert!(err.to_string().contains("owner/repo shorthand"));
    }

    #[test]
    fn normalize_dependency_source_requires_provider_for_shorthand() {
        let err = normalize_dependency_source("owner/repo", None).unwrap_err();
        assert!(err.to_string().contains("default git provider"));
    }

    #[test]
    fn normalize_dependency_source_rejects_insecure_provider_url() {
        let err =
            normalize_dependency_source("owner/repo", Some("http://example.com")).unwrap_err();
        assert!(err.to_string().contains("insecure HTTP"));
    }

    #[test]
    fn resolve_core_plugin_name_handles_short_aliases_and_typos() {
        assert_eq!(
            resolve_core_plugin_name("polars"),
            Some("nu_plugin_polars".to_string())
        );
        assert_eq!(
            resolve_core_plugin_name("polors"),
            Some("nu_plugin_polars".to_string())
        );
        assert_eq!(
            resolve_core_plugin_name("gstat"),
            Some("nu_plugin_gstat".to_string())
        );
    }

    #[test]
    fn resolve_core_plugin_name_accepts_full_name() {
        assert_eq!(
            resolve_core_plugin_name("nu_plugin_query"),
            Some("nu_plugin_query".to_string())
        );
    }

    #[test]
    fn list_installed_module_names_returns_sorted_directories_only() {
        let modules_dir = make_temp_dir("list_modules");
        std::fs::create_dir_all(modules_dir.join("nu-zeta")).unwrap();
        std::fs::create_dir_all(modules_dir.join("nu-alpha")).unwrap();
        std::fs::create_dir_all(modules_dir.join("nu-alpha-v0.1.0.dist-info")).unwrap();
        std::fs::write(modules_dir.join("activate.nu"), "# generated").unwrap();

        let modules = list_installed_module_names(&modules_dir).unwrap();

        assert_eq!(modules, vec!["nu-alpha".to_string(), "nu-zeta".to_string()]);

        let _ = std::fs::remove_dir_all(modules_dir);
    }

    #[test]
    fn list_installed_module_names_handles_missing_directory() {
        let root_dir = make_temp_dir("list_missing");
        let modules = list_installed_module_names(&root_dir.join("missing")).unwrap();
        assert!(modules.is_empty());
        let _ = std::fs::remove_dir_all(root_dir);
    }

    #[test]
    fn remove_module_dist_info_dirs_removes_only_matching_module_dist_info() {
        let modules_dir = make_temp_dir("remove_dist_info");
        std::fs::create_dir_all(modules_dir.join("nu-salesforce")).unwrap();
        std::fs::create_dir_all(modules_dir.join("nu-salesforce-v0.3.0.dist-info")).unwrap();
        std::fs::create_dir_all(modules_dir.join("nu-salesforce-abcdef123456.dist-info")).unwrap();
        std::fs::create_dir_all(modules_dir.join("nu-other-v1.0.0.dist-info")).unwrap();
        std::fs::create_dir_all(modules_dir.join("nu-salesforce.dist-info")).unwrap();

        let removed = remove_module_dist_info_dirs(&modules_dir, "nu-salesforce").unwrap();

        assert_eq!(
            removed,
            vec![
                "nu-salesforce-abcdef123456.dist-info".to_string(),
                "nu-salesforce-v0.3.0.dist-info".to_string()
            ]
        );
        assert!(modules_dir.join("nu-salesforce").is_dir());
        assert!(modules_dir.join("nu-other-v1.0.0.dist-info").is_dir());
        assert!(modules_dir.join("nu-salesforce.dist-info").is_dir());
        assert!(!modules_dir.join("nu-salesforce-v0.3.0.dist-info").exists());
        assert!(
            !modules_dir
                .join("nu-salesforce-abcdef123456.dist-info")
                .exists()
        );

        let _ = std::fs::remove_dir_all(modules_dir);
    }

    #[test]
    fn list_installed_plugin_names_returns_sorted_plugin_binaries() {
        let bin_dir = make_temp_dir("list_plugins");
        std::fs::write(bin_dir.join("nu_plugin_query"), "binary").unwrap();
        std::fs::write(bin_dir.join("nu_plugin_polars"), "binary").unwrap();
        std::fs::write(bin_dir.join("nu"), "binary").unwrap(); // should be excluded
        std::fs::write(bin_dir.join("other_file"), "binary").unwrap(); // should be excluded

        let plugins = list_installed_plugin_names(&bin_dir).unwrap();

        assert_eq!(
            plugins,
            vec![
                "nu_plugin_polars".to_string(),
                "nu_plugin_query".to_string()
            ]
        );

        let _ = std::fs::remove_dir_all(bin_dir);
    }

    #[test]
    fn list_installed_plugin_names_handles_missing_directory() {
        let root_dir = make_temp_dir("list_plugins_missing");
        let plugins = list_installed_plugin_names(&root_dir.join("missing")).unwrap();
        assert!(plugins.is_empty());
        let _ = std::fs::remove_dir_all(root_dir);
    }

    #[test]
    fn resolve_run_invocation_injects_config_for_nu() {
        let cwd = make_temp_dir("run_inject_nu");
        let config = cwd.join(".nu-env").join("config.nu");
        let plugin_config = cwd.join(".nu-env").join("plugins.msgpackz");
        let command = vec![
            "nu".to_string(),
            "script.nu".to_string(),
            "--flag".to_string(),
        ];

        let (program, args) = resolve_run_invocation(&command, &config, &plugin_config, &cwd);

        assert_eq!(program, "nu");
        assert_eq!(args[0], "--config");
        assert_eq!(args[1], config.to_string_lossy());
        assert_eq!(args[2], "--plugin-config");
        assert_eq!(args[3], plugin_config.to_string_lossy());
        assert_eq!(args[4], "script.nu");
        assert_eq!(args[5], "--flag");

        let _ = std::fs::remove_dir_all(cwd);
    }

    #[test]
    fn resolve_run_invocation_preserves_existing_plugin_config_for_nu() {
        let cwd = make_temp_dir("run_existing_plugin_config");
        let config = cwd.join(".nu-env").join("config.nu");
        let plugin_config = cwd.join(".nu-env").join("plugins.msgpackz");
        let custom_plugin_config = cwd.join("custom_plugins.msgpackz");
        let command = vec![
            "nu".to_string(),
            "--plugin-config".to_string(),
            custom_plugin_config.to_string_lossy().to_string(),
            "script.nu".to_string(),
        ];

        let (program, args) = resolve_run_invocation(&command, &config, &plugin_config, &cwd);

        assert_eq!(program, "nu");
        assert_eq!(args[0], "--config");
        assert_eq!(args[1], config.to_string_lossy());
        assert_eq!(args[2], "--plugin-config");
        assert_eq!(args[3], custom_plugin_config.to_string_lossy());
        assert_eq!(args[4], "script.nu");

        let _ = std::fs::remove_dir_all(cwd);
    }

    #[test]
    fn resolve_run_invocation_wraps_nu_script() {
        let cwd = make_temp_dir("run_wrap_script");
        let script_path = cwd.join("tool.nu");
        std::fs::write(&script_path, "print 'ok'").unwrap();
        let config = cwd.join(".nu-env").join("config.nu");
        let plugin_config = cwd.join(".nu-env").join("plugins.msgpackz");
        let command = vec!["tool.nu".to_string(), "arg1".to_string()];

        let (program, args) = resolve_run_invocation(&command, &config, &plugin_config, &cwd);

        assert_eq!(program, "nu");
        assert_eq!(args[0], "--config");
        assert_eq!(args[1], config.to_string_lossy());
        assert_eq!(args[2], "--plugin-config");
        assert_eq!(args[3], plugin_config.to_string_lossy());
        assert_eq!(args[4], "tool.nu");
        assert_eq!(args[5], "arg1");

        let _ = std::fs::remove_dir_all(cwd);
    }

    #[test]
    fn resolve_run_invocation_leaves_other_commands_unchanged() {
        let cwd = make_temp_dir("run_other_cmd");
        let config = cwd.join(".nu-env").join("config.nu");
        let plugin_config = cwd.join(".nu-env").join("plugins.msgpackz");
        let command = vec!["echo".to_string(), "hello".to_string()];

        let (program, args) = resolve_run_invocation(&command, &config, &plugin_config, &cwd);

        assert_eq!(program, "echo");
        assert_eq!(args, vec!["hello".to_string()]);

        let _ = std::fs::remove_dir_all(cwd);
    }

    #[test]
    fn cmd_run_requires_manifest() {
        let cwd = make_temp_dir("run_requires_manifest");
        let err = cmd_run(
            &cwd,
            vec!["nu".to_string(), "-c".to_string(), "print hi".to_string()],
        )
        .unwrap_err();

        assert!(matches!(err, error::QuiverError::NoManifest(_)));

        let _ = std::fs::remove_dir_all(cwd);
    }

    #[test]
    fn init_creates_mod_nu_in_subdir_named_after_current_dir() {
        let project_dir = make_temp_dir("init_subdir");
        let store = detect_nu_version().map(|version| seed_fake_nu_store(&version));

        cmd_init(&project_dir, None, "0.1.0".to_string(), None, None).unwrap();

        let dir_name = project_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap()
            .to_string();
        assert!(project_dir.join("nupackage.toml").exists());
        assert!(project_dir.join(&dir_name).join("mod.nu").exists());
        assert!(!project_dir.join("mod.nu").exists());

        // Verify .nu-env files are generated
        let nu_env = project_dir.join(".nu-env");
        assert!(nu_env.join("activate.nu").exists());
        assert!(nu_env.join("config.nu").exists());
        assert!(nu_env.join("modules").is_dir());

        let activate = std::fs::read_to_string(nu_env.join("activate.nu")).unwrap();
        assert!(activate.contains("export-env {"));
        assert!(activate.contains("source-env"));
        assert!(activate.contains("export alias nu = ^"));
        assert!(activate.contains("export alias deactivate = overlay hide activate"));

        let config_nu = std::fs::read_to_string(nu_env.join("config.nu")).unwrap();
        assert!(config_nu.contains("export const NU_LIB_DIRS"));
        assert!(config_nu.contains(".nu-env/modules"));

        let _ = std::fs::remove_dir_all(project_dir);
        if let Some((store_root, _guard)) = store {
            let _ = std::fs::remove_dir_all(store_root);
        }
    }

    #[test]
    fn init_uses_git_user_name_for_authors_when_available() {
        let project_dir = make_temp_dir("init_git_author");
        let (git_dir, _path_guard) = seed_fake_git_config_user_name("Alice Example");

        cmd_init(&project_dir, None, "0.1.0".to_string(), None, None).unwrap();

        let manifest_text = std::fs::read_to_string(project_dir.join("nupackage.toml")).unwrap();
        let manifest = Manifest::from_str(&manifest_text).unwrap();
        assert_eq!(
            manifest.package.authors,
            Some(vec!["Alice Example".to_string()])
        );
        assert!(manifest_text.contains("authors = [\"Alice Example\"]"));

        let _ = std::fs::remove_dir_all(project_dir);
        let _ = std::fs::remove_dir_all(git_dir);
    }

    #[test]
    fn init_omits_authors_when_git_user_name_is_empty() {
        let project_dir = make_temp_dir("init_no_git_author");
        let (git_dir, _path_guard) = seed_fake_git_config_user_name("");

        cmd_init(&project_dir, None, "0.1.0".to_string(), None, None).unwrap();

        let manifest_text = std::fs::read_to_string(project_dir.join("nupackage.toml")).unwrap();
        let manifest = Manifest::from_str(&manifest_text).unwrap();
        assert_eq!(manifest.package.authors, None);
        assert!(!manifest_text.contains("authors ="));

        let _ = std::fs::remove_dir_all(project_dir);
        let _ = std::fs::remove_dir_all(git_dir);
    }

    #[test]
    fn init_respects_explicit_name_but_uses_current_dir_for_mod_nu_subdir() {
        let project_dir = make_temp_dir("init_named_subdir");
        let store = detect_nu_version().map(|version| seed_fake_nu_store(&version));

        cmd_init(
            &project_dir,
            Some("custom-module-name".to_string()),
            "0.1.0".to_string(),
            None,
            None,
        )
        .unwrap();

        let manifest_text = std::fs::read_to_string(project_dir.join("nupackage.toml")).unwrap();
        let manifest = Manifest::from_str(&manifest_text).unwrap();
        assert_eq!(manifest.package.name, "custom-module-name");

        let dir_name = project_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap()
            .to_string();
        assert!(project_dir.join(&dir_name).join("mod.nu").exists());
        assert!(!project_dir.join("mod.nu").exists());

        // Verify .nu-env files are generated
        assert!(project_dir.join(".nu-env").join("activate.nu").exists());
        assert!(project_dir.join(".nu-env").join("config.nu").exists());

        let _ = std::fs::remove_dir_all(project_dir);
        if let Some((store_root, _guard)) = store {
            let _ = std::fs::remove_dir_all(store_root);
        }
    }

    #[test]
    fn init_uses_explicit_nu_version_when_provided() {
        let project_dir = make_temp_dir("init_nu_version");
        let local_nu_version = "255.255.255".to_string();
        let (store_root, _guard) = seed_fake_nu_store(&local_nu_version);

        cmd_init(
            &project_dir,
            None,
            "0.1.0".to_string(),
            Some(local_nu_version.clone()),
            None,
        )
        .unwrap();

        let manifest_text = std::fs::read_to_string(project_dir.join("nupackage.toml")).unwrap();
        let manifest = Manifest::from_str(&manifest_text).unwrap();
        assert_eq!(
            manifest.package.nu_version.as_deref(),
            Some(local_nu_version.as_str())
        );

        let _ = std::fs::remove_dir_all(project_dir);
        let _ = std::fs::remove_dir_all(store_root);
    }

    #[test]
    fn init_rejects_invalid_explicit_nu_version() {
        let project_dir = make_temp_dir("init_bad_nu_version");

        let err = cmd_init(
            &project_dir,
            None,
            "0.1.0".to_string(),
            Some("definitely-not-semver".to_string()),
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("package nu-version"));

        let _ = std::fs::remove_dir_all(project_dir);
    }

    #[test]
    fn init_creates_gitignore_with_nu_env_entry() {
        let project_dir = make_temp_dir("init_gitignore_create");
        let store = detect_nu_version().map(|version| seed_fake_nu_store(&version));

        cmd_init(&project_dir, None, "0.1.0".to_string(), None, None).unwrap();

        let gitignore = std::fs::read_to_string(project_dir.join(".gitignore")).unwrap();
        assert!(gitignore.lines().any(|line| line.trim() == ".nu-env/"));

        let _ = std::fs::remove_dir_all(project_dir);
        if let Some((store_root, _guard)) = store {
            let _ = std::fs::remove_dir_all(store_root);
        }
    }

    #[test]
    fn init_appends_nu_env_entry_to_existing_gitignore() {
        let project_dir = make_temp_dir("init_gitignore_append");
        let store = detect_nu_version().map(|version| seed_fake_nu_store(&version));
        std::fs::write(project_dir.join(".gitignore"), "target/\n").unwrap();

        cmd_init(&project_dir, None, "0.1.0".to_string(), None, None).unwrap();

        let gitignore = std::fs::read_to_string(project_dir.join(".gitignore")).unwrap();
        assert!(gitignore.lines().any(|line| line.trim() == "target/"));
        assert!(gitignore.lines().any(|line| line.trim() == ".nu-env/"));

        let _ = std::fs::remove_dir_all(project_dir);
        if let Some((store_root, _guard)) = store {
            let _ = std::fs::remove_dir_all(store_root);
        }
    }

    #[test]
    fn helix_lsp_config_generates_languages_toml() {
        let project_dir = make_temp_dir("helix_lsp");

        write_helix_lsp_config(&project_dir).unwrap();

        let config_path = project_dir.join(".helix").join("languages.toml");
        assert!(config_path.exists());

        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("[language-server.nu-lsp]"));
        assert!(content.contains("command = \".nu-env/bin/nu\""));
        assert!(content.contains("--config .nu-env/config.nu"));
        assert!(content.contains("--plugin-config .nu-env/plugins.msgpackz"));
        assert!(content.contains("--lsp"));

        let _ = std::fs::remove_dir_all(project_dir);
    }

    #[test]
    fn zed_lsp_config_generates_settings_json() {
        let project_dir = make_temp_dir("zed_lsp");

        write_zed_lsp_config(&project_dir).unwrap();

        let config_path = project_dir.join(".zed").join("settings.json");
        assert!(config_path.exists());

        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("\"nu\""));
        assert!(content.contains("\"path\": \".nu-env/bin/nu\""));
        assert!(content.contains("--config"));
        assert!(content.contains(".nu-env/config.nu"));
        assert!(content.contains("--plugin-config"));
        assert!(content.contains(".nu-env/plugins.msgpackz"));
        assert!(content.contains("--lsp"));

        let _ = std::fs::remove_dir_all(project_dir);
    }

    #[test]
    fn cmd_lsp_with_explicit_editors_generates_configs() {
        let project_dir = make_temp_dir("lsp_explicit");

        cmd_lsp(&project_dir, vec!["helix".to_string(), "zed".to_string()]).unwrap();

        assert!(project_dir.join(".helix").join("languages.toml").exists());
        assert!(project_dir.join(".zed").join("settings.json").exists());

        let _ = std::fs::remove_dir_all(project_dir);
    }

    #[test]
    fn cmd_lsp_rejects_unknown_editor() {
        let project_dir = make_temp_dir("lsp_unknown");

        let err = cmd_lsp(&project_dir, vec!["vim".to_string()]).unwrap_err();
        assert!(err.to_string().contains("unknown editor"));

        let _ = std::fs::remove_dir_all(project_dir);
    }
}
