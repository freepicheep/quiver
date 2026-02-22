mod checksum;
mod cli;
mod config;
mod error;
mod git;
mod installer;
mod lockfile;
mod manifest;
mod resolver;

use std::path::Path;

use cli::Commands;
use config::GlobalConfig;
use error::Result;
use manifest::{DependencySpec, Manifest, Package, ScriptDependencySpec};

fn main() {
    let cli = cli::parse();

    if let Err(e) = run(cli.command) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(command: Commands) -> Result<()> {
    let cwd = std::env::current_dir()?;

    match command {
        Commands::Init {
            name,
            version,
            description,
        } => cmd_init(&cwd, name, version, description),
        Commands::Install { global, frozen } => {
            if global {
                cmd_install_global(frozen)
            } else {
                cmd_install(&cwd, frozen)
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
        Commands::AddScript {
            url,
            path,
            name,
            tag,
            rev,
            branch,
        } => cmd_add_script(&cwd, url, path, name, tag, rev, branch),
        Commands::Remove { global, name } => {
            if global {
                cmd_remove_global(name)
            } else {
                cmd_remove(&cwd, name)
            }
        }
        Commands::RemoveScript { name } => cmd_remove_script(&cwd, name),
        Commands::List => cmd_list(&cwd),
        Commands::Version => cmd_version(),
        Commands::Hook => cmd_hook(),
    }
}

fn cmd_init(
    dir: &Path,
    name: Option<String>,
    version: String,
    description: Option<String>,
) -> Result<()> {
    let mod_toml = dir.join("mod.toml");
    if mod_toml.exists() {
        return Err(error::NuanceError::Manifest(
            "mod.toml already exists in this directory".to_string(),
        ));
    }

    // Default name to directory name
    let pkg_name = name.unwrap_or_else(|| {
        dir.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("my-module")
            .to_string()
    });

    let manifest = Manifest {
        package: Package {
            name: pkg_name.clone(),
            version,
            description,
            license: None,
            authors: None,
            nu_version: None,
        },
        dependencies: Default::default(),
    };

    let content = manifest.to_toml_string()?;
    std::fs::write(&mod_toml, content)?;
    eprintln!("Created mod.toml for '{pkg_name}'");

    // Also create mod.nu if it doesn't exist
    let mod_nu = dir.join("mod.nu");
    if !mod_nu.exists() {
        std::fs::write(
            &mod_nu,
            "# Module entry point\n# Export your commands here with: export use <submodule>\n",
        )?;
        eprintln!("Created mod.nu");
    }

    Ok(())
}

fn cmd_install(dir: &Path, frozen: bool) -> Result<()> {
    installer::install(dir, frozen)
}

fn cmd_install_global(frozen: bool) -> Result<()> {
    installer::install_global(frozen)
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
        error::NuanceError::Other(format!("could not determine package name from URL: {url}"))
    })?;

    // Check if already added as either module or script
    if manifest.dependencies.modules.contains_key(&pkg_name)
        || manifest.dependencies.scripts.contains_key(&pkg_name)
    {
        return Err(error::NuanceError::Manifest(format!(
            "dependency '{pkg_name}' already exists in mod.toml"
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
    std::fs::write(dir.join("mod.toml"), content)?;

    eprintln!("Added module '{pkg_name}' to mod.toml");

    // Run install
    installer::install(dir, false)
}

fn cmd_add_script(
    dir: &Path,
    url: String,
    path: Option<String>,
    name: Option<String>,
    tag: Option<String>,
    rev: Option<String>,
    branch: Option<String>,
) -> Result<()> {
    let mut manifest = Manifest::from_dir(dir)?;
    let provider_base = if is_git_url(url.trim()) {
        None
    } else {
        let config = GlobalConfig::load_or_default()?;
        Some(config.default_git_provider_base_url()?)
    };
    let source = normalize_script_source(&url, path, provider_base.as_deref())?;

    let mut tag = tag;
    let mut rev = rev;
    let mut branch = branch;
    if tag.is_none() && rev.is_none() && branch.is_none() {
        if let Some(ref_spec) = source.inferred_ref.as_deref() {
            let (inferred_tag, inferred_rev, inferred_branch) =
                infer_ref_fields_from_spec(&source.url, ref_spec)?;
            tag = inferred_tag;
            rev = inferred_rev;
            branch = inferred_branch;
        }
    }

    let dep_name = match name {
        Some(explicit) => explicit,
        None => script_name_from_path(&source.path)?,
    };

    if manifest.dependencies.modules.contains_key(&dep_name)
        || manifest.dependencies.scripts.contains_key(&dep_name)
    {
        return Err(error::NuanceError::Manifest(format!(
            "dependency '{dep_name}' already exists in mod.toml"
        )));
    }

    let script_spec = auto_detect_script_dep_spec(&source.url, &source.path, tag, rev, branch)?;
    script_spec.validate(&dep_name)?;

    manifest
        .dependencies
        .scripts
        .insert(dep_name.clone(), script_spec);

    let content = manifest.to_toml_string()?;
    std::fs::write(dir.join("mod.toml"), content)?;

    eprintln!("Added script '{dep_name}' to mod.toml");
    installer::install(dir, false)
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
        error::NuanceError::Other(format!("could not determine package name from URL: {url}"))
    })?;

    // Check if already added
    if config.dependencies.contains_key(&pkg_name) {
        return Err(error::NuanceError::Config(format!(
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
    installer::install_global(false)
}

fn cmd_remove(dir: &Path, name: String) -> Result<()> {
    // Load existing manifest
    let mut manifest = Manifest::from_dir(dir)?;

    // Check the module dep exists
    if manifest.dependencies.modules.remove(&name).is_none() {
        return Err(error::NuanceError::Manifest(format!(
            "module dependency '{name}' not found in mod.toml"
        )));
    }

    // Write updated manifest
    let content = manifest.to_toml_string()?;
    std::fs::write(dir.join("mod.toml"), content)?;
    eprintln!("Removed module '{name}' from mod.toml");

    // Remove from .nu_modules/
    let module_dir = dir.join(".nu_modules").join(&name);
    if module_dir.exists() {
        std::fs::remove_dir_all(&module_dir)?;
        eprintln!("Removed .nu_modules/{name}/");
    }

    // Update lockfile: remove the package entry
    let lock_path = dir.join("mod.lock");
    if lock_path.exists() {
        let mut lockfile = lockfile::Lockfile::from_path(&lock_path)?;
        lockfile
            .packages
            .retain(|p| !(p.name == name && p.kind == lockfile::LockedPackageKind::Module));
        lockfile.write_to(&lock_path)?;
        eprintln!("Updated mod.lock");
    }

    // Regenerate activate.nu from the updated manifest and lockfile state.
    eprintln!("Regenerating activate.nu...");
    installer::install(dir, false)?;

    Ok(())
}

fn cmd_remove_script(dir: &Path, name: String) -> Result<()> {
    let mut manifest = Manifest::from_dir(dir)?;

    if manifest.dependencies.scripts.remove(&name).is_none() {
        return Err(error::NuanceError::Manifest(format!(
            "script dependency '{name}' not found in mod.toml"
        )));
    }

    let content = manifest.to_toml_string()?;
    std::fs::write(dir.join("mod.toml"), content)?;
    eprintln!("Removed script '{name}' from mod.toml");

    let script_path = dir.join(".nu_scripts").join(format!("{name}.nu"));
    if script_path.exists() {
        std::fs::remove_file(&script_path)?;
        eprintln!("Removed {}", script_path.display());
    }

    let lock_path = dir.join("mod.lock");
    if lock_path.exists() {
        let mut lockfile = lockfile::Lockfile::from_path(&lock_path)?;
        lockfile
            .packages
            .retain(|p| !(p.name == name && p.kind == lockfile::LockedPackageKind::Script));
        lockfile.write_to(&lock_path)?;
        eprintln!("Updated mod.lock");
    }

    eprintln!("Regenerating activate.nu...");
    installer::install(dir, false)?;

    Ok(())
}

fn cmd_remove_global(name: String) -> Result<()> {
    let mut config = GlobalConfig::load()?;

    // Check the dep exists
    if config.dependencies.remove(&name).is_none() {
        return Err(error::NuanceError::Config(format!(
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
    installer::install_global(false)?;

    Ok(())
}

fn cmd_hook() -> Result<()> {
    let hook_script = r#"# nuance auto-activate hook — add this to your config.nu (or env.nu)
$env.config.hooks.env_change.PWD = (
    $env.config.hooks.env_change.PWD | default [] | append {|before, after|
        # Remove previous directory's modules/scripts if it was a nuance project
        if ($before | path join "mod.toml" | path exists) {
            let old_modules = ($before | path join ".nu_modules")
            let old_scripts = ($before | path join ".nu_scripts")
            $env.NU_LIB_DIRS = ($env.NU_LIB_DIRS | default [] | where { |it| $it != $old_modules and $it != $old_scripts })
        }
        # Add new directory's modules/scripts if it is a nuance project
        if ($after | path join "mod.toml" | path exists) {
            let new_modules = ($after | path join ".nu_modules")
            let new_scripts = ($after | path join ".nu_scripts")
            if ($new_modules | path exists) and ($new_modules not-in ($env.NU_LIB_DIRS | default [])) {
                $env.NU_LIB_DIRS = ($env.NU_LIB_DIRS | default [] | append $new_modules)
            }
            if ($new_scripts | path exists) and ($new_scripts not-in ($env.NU_LIB_DIRS | default [])) {
                $env.NU_LIB_DIRS = ($env.NU_LIB_DIRS | default [] | append $new_scripts)
            }
        }
    }
)"#;
    println!("{hook_script}");
    Ok(())
}

fn cmd_version() -> Result<()> {
    println!("nuance {}", env!("CARGO_PKG_VERSION"));
    Ok(())
}

fn cmd_list(cwd: &Path) -> Result<()> {
    if cwd.join("mod.toml").exists() {
        let modules_dir = cwd.join(".nu_modules");
        let scripts_dir = cwd.join(".nu_scripts");
        let modules = list_installed_module_names(&modules_dir)?;
        let scripts = list_installed_script_names(&scripts_dir)?;

        if modules.is_empty() && scripts.is_empty() {
            eprintln!(
                "No installed project dependencies found in {} or {}",
                modules_dir.display(),
                scripts_dir.display()
            );
            return Ok(());
        }

        if !modules.is_empty() {
            eprintln!("Installed project modules from {}", modules_dir.display());
            for module in modules {
                eprintln!("{module}");
            }
        }

        if !scripts.is_empty() {
            eprintln!("Installed project scripts from {}", scripts_dir.display());
            for script in scripts {
                eprintln!("{script}");
            }
        }
    } else {
        let config = GlobalConfig::load_or_default()?;
        let modules_dir = config.modules_dir()?;
        let modules = list_installed_module_names(&modules_dir)?;

        if modules.is_empty() {
            eprintln!(
                "No installed global modules found in {}",
                modules_dir.display()
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
            modules.push(name.to_string());
        }
    }

    modules.sort();
    Ok(modules)
}

fn list_installed_script_names(scripts_dir: &Path) -> Result<Vec<String>> {
    if !scripts_dir.exists() {
        return Ok(Vec::new());
    }

    let mut scripts = Vec::new();

    for entry in std::fs::read_dir(scripts_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("nu") {
            continue;
        }

        if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
            scripts.push(stem.to_string());
        }
    }

    scripts.sort();
    Ok(scripts)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalizedScriptSource {
    url: String,
    path: String,
    inferred_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedBlobScriptUrl {
    repo_url: String,
    ref_spec: String,
    path: String,
}

fn normalize_script_source(
    input_url: &str,
    input_path: Option<String>,
    provider_base_url: Option<&str>,
) -> Result<NormalizedScriptSource> {
    if let Some(blob) = parse_blob_script_url(input_url) {
        if let Some(explicit_path) = input_path {
            if explicit_path != blob.path {
                return Err(error::NuanceError::Other(format!(
                    "script path '{explicit_path}' does not match path '{}' in blob URL",
                    blob.path
                )));
            }
        }

        return Ok(NormalizedScriptSource {
            url: blob.repo_url,
            path: blob.path,
            inferred_ref: Some(blob.ref_spec),
        });
    }

    let path = input_path.ok_or_else(|| {
        error::NuanceError::Other(
            "missing script path; use `nuance add-script <repo> <path>` or pass a full blob URL"
                .to_string(),
        )
    })?;
    let url = normalize_dependency_source(input_url, provider_base_url)?;

    Ok(NormalizedScriptSource {
        url,
        path,
        inferred_ref: None,
    })
}

fn parse_blob_script_url(input: &str) -> Option<ParsedBlobScriptUrl> {
    let trimmed = input.trim();
    let no_fragment = trimmed.split('#').next().unwrap_or(trimmed);
    let no_query = no_fragment.split('?').next().unwrap_or(no_fragment);

    let scheme_sep = no_query.find("://")?;
    let scheme = &no_query[..scheme_sep];
    let rest = &no_query[(scheme_sep + 3)..];
    let slash_idx = rest.find('/')?;
    let host = &rest[..slash_idx];
    let path = &rest[(slash_idx + 1)..];

    let segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    let blob_idx = segments.iter().position(|segment| *segment == "blob")?;

    let repo_end = if blob_idx > 0 && segments[blob_idx - 1] == "-" {
        blob_idx - 1
    } else {
        blob_idx
    };

    if repo_end < 2 {
        return None;
    }

    let ref_spec = segments.get(blob_idx + 1)?;
    let script_segments = segments.get((blob_idx + 2)..)?;
    if script_segments.is_empty() {
        return None;
    }

    let repo_path = segments[..repo_end].join("/");
    let path = script_segments.join("/");

    Some(ParsedBlobScriptUrl {
        repo_url: format!("{scheme}://{host}/{repo_path}"),
        ref_spec: decode_blob_ref_segment(ref_spec),
        path,
    })
}

fn decode_blob_ref_segment(value: &str) -> String {
    value.replace("%2F", "/").replace("%2f", "/")
}

fn infer_ref_fields_from_spec(
    url: &str,
    ref_spec: &str,
) -> Result<(Option<String>, Option<String>, Option<String>)> {
    let repo_path = git::clone_or_fetch(url)?;

    if git::resolve_ref(&repo_path, ref_spec, git::RefKind::Rev).is_ok() {
        return Ok((None, Some(ref_spec.to_string()), None));
    }
    if git::resolve_ref(&repo_path, ref_spec, git::RefKind::Tag).is_ok() {
        return Ok((Some(ref_spec.to_string()), None, None));
    }
    if git::resolve_ref(&repo_path, ref_spec, git::RefKind::Branch).is_ok() {
        return Ok((None, None, Some(ref_spec.to_string())));
    }

    Err(error::NuanceError::Other(format!(
        "could not resolve blob ref '{ref_spec}' for repository {url}; pass --tag/--branch/--rev explicitly"
    )))
}

fn normalize_dependency_source(input: &str, provider_base_url: Option<&str>) -> Result<String> {
    let trimmed = input.trim();

    if trimmed.is_empty() {
        return Err(error::NuanceError::Other(
            "dependency source cannot be empty".to_string(),
        ));
    }

    if is_git_url(trimmed) {
        return Ok(trimmed.to_string());
    }

    if is_repo_shorthand(trimmed) {
        let provider_base = provider_base_url.ok_or_else(|| {
            error::NuanceError::Other(
                "a default git provider is required for owner/repo shorthand".to_string(),
            )
        })?;
        return Ok(format!("{provider_base}/{trimmed}"));
    }

    Err(error::NuanceError::Other(format!(
        "invalid dependency source '{input}'; expected a git URL or owner/repo shorthand"
    )))
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
        eprintln!("Fetching {url} to detect version...");
        let repo_path = git::clone_or_fetch(url)?;

        if let Some(latest) = git::latest_tag(&repo_path)? {
            eprintln!("  Found latest tag: {latest}");
            Ok(DependencySpec {
                git: url.to_string(),
                tag: Some(latest),
                rev: None,
                branch: None,
            })
        } else {
            let default_br = git::default_branch(&repo_path)?;
            eprintln!("  No tags found, using branch: {default_br}");
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

/// Auto-detect a script dependency spec from a URL + path, optionally with an explicit ref.
fn auto_detect_script_dep_spec(
    url: &str,
    path: &str,
    tag: Option<String>,
    rev: Option<String>,
    branch: Option<String>,
) -> Result<ScriptDependencySpec> {
    if tag.is_none() && rev.is_none() && branch.is_none() {
        eprintln!("Fetching {url} to detect version...");
        let repo_path = git::clone_or_fetch(url)?;

        if let Some(latest) = git::latest_tag(&repo_path)? {
            eprintln!("  Found latest tag: {latest}");
            Ok(ScriptDependencySpec {
                git: url.to_string(),
                path: path.to_string(),
                tag: Some(latest),
                rev: None,
                branch: None,
            })
        } else {
            let default_br = git::default_branch(&repo_path)?;
            eprintln!("  No tags found, using branch: {default_br}");
            Ok(ScriptDependencySpec {
                git: url.to_string(),
                path: path.to_string(),
                tag: None,
                rev: None,
                branch: Some(default_br),
            })
        }
    } else {
        Ok(ScriptDependencySpec {
            git: url.to_string(),
            path: path.to_string(),
            tag,
            rev,
            branch,
        })
    }
}

fn script_name_from_path(path: &str) -> Result<String> {
    let stem = Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| {
            error::NuanceError::Manifest(format!(
                "could not derive script name from path '{path}'; use --name"
            ))
        })?;

    if stem.trim().is_empty() {
        return Err(error::NuanceError::Manifest(format!(
            "could not derive script name from path '{path}'; use --name"
        )));
    }

    Ok(stem.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn config_with_provider(provider: &str) -> GlobalConfig {
        GlobalConfig {
            modules_dir: None,
            default_git_provider: provider.to_string(),
            dependencies: HashMap::new(),
        }
    }

    fn make_temp_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "nuance_main_test_{}_{}_{}",
            label,
            std::process::id(),
            unique
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn normalize_dependency_source_passes_through_urls() {
        let https = normalize_dependency_source("https://example.com/team/repo", None).unwrap();
        assert_eq!(https, "https://example.com/team/repo");

        let ssh = normalize_dependency_source("git@github.com:user/repo.git", None).unwrap();
        assert_eq!(ssh, "git@github.com:user/repo.git");
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
    fn parse_blob_script_url_extracts_repo_ref_and_path() {
        let parsed = parse_blob_script_url(
            "https://github.com/nushell/nu_scripts/blob/main/sourced/webscraping/twitter.nu",
        )
        .unwrap();

        assert_eq!(parsed.repo_url, "https://github.com/nushell/nu_scripts");
        assert_eq!(parsed.ref_spec, "main");
        assert_eq!(parsed.path, "sourced/webscraping/twitter.nu");
    }

    #[test]
    fn parse_blob_script_url_handles_gitlab_dash_blob_form() {
        let parsed = parse_blob_script_url(
            "https://gitlab.com/group/subgroup/repo/-/blob/feature%2Fnew/scripts/task.nu",
        )
        .unwrap();

        assert_eq!(parsed.repo_url, "https://gitlab.com/group/subgroup/repo");
        assert_eq!(parsed.ref_spec, "feature/new");
        assert_eq!(parsed.path, "scripts/task.nu");
    }

    #[test]
    fn normalize_script_source_accepts_blob_url_without_path_arg() {
        let normalized = normalize_script_source(
            "https://github.com/nushell/nu_scripts/blob/main/sourced/webscraping/twitter.nu",
            None,
            None,
        )
        .unwrap();

        assert_eq!(normalized.url, "https://github.com/nushell/nu_scripts");
        assert_eq!(normalized.path, "sourced/webscraping/twitter.nu");
        assert_eq!(normalized.inferred_ref.as_deref(), Some("main"));
    }

    #[test]
    fn normalize_script_source_rejects_path_mismatch_with_blob_url() {
        let err = normalize_script_source(
            "https://github.com/nushell/nu_scripts/blob/main/sourced/webscraping/twitter.nu",
            Some("scripts/other.nu".to_string()),
            None,
        )
        .unwrap_err();

        assert!(err.to_string().contains("does not match path"));
    }

    #[test]
    fn normalize_script_source_requires_path_for_non_blob_source() {
        let err = normalize_script_source("https://github.com/nushell/nu_scripts", None, None)
            .unwrap_err();
        assert!(err.to_string().contains("missing script path"));
    }

    #[test]
    fn list_installed_module_names_returns_sorted_directories_only() {
        let modules_dir = make_temp_dir("list_modules");
        std::fs::create_dir_all(modules_dir.join("nu-zeta")).unwrap();
        std::fs::create_dir_all(modules_dir.join("nu-alpha")).unwrap();
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
    fn list_installed_script_names_returns_sorted_nu_files_only() {
        let scripts_dir = make_temp_dir("list_scripts");
        std::fs::write(scripts_dir.join("zeta.nu"), "def main [] {}").unwrap();
        std::fs::write(scripts_dir.join("alpha.nu"), "def main [] {}").unwrap();
        std::fs::write(scripts_dir.join("notes.txt"), "ignore").unwrap();
        std::fs::create_dir_all(scripts_dir.join("nested")).unwrap();

        let scripts = list_installed_script_names(&scripts_dir).unwrap();
        assert_eq!(scripts, vec!["alpha".to_string(), "zeta".to_string()]);

        let _ = std::fs::remove_dir_all(scripts_dir);
    }

    #[test]
    fn script_name_from_path_uses_file_stem() {
        let name = script_name_from_path("scripts/quickfix.nu").unwrap();
        assert_eq!(name, "quickfix");
    }
}
