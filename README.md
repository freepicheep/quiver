<!-- LOGO -->
<h1>
<p align="center">
  <img src="https://github.com/user-attachments/assets/5bd69aeb-3c42-4a5e-8281-65b18748a43c" alt="Logo" width="128">
  <br>Quiver
</h1>
  <p align="center">
    A fast package and project manager for <a href="https://www.nushell.sh/">Nushell</a> modules and plugins.
    <br />
  </p>
</p>

Quiver handles dependency resolution, fetching, and lockfile management for Nushell modules and plugins distributed as git repositories.

## Install

Quiver is pre-alpha. I release breaking changes frequently. Most of the code is written with Codex 5.3. I release build for the following platforms and have confirmed quiver works great on macOS silicon and ARM64 Linux (thanks Asahi devs).

![Apple Silicon macOS](https://img.shields.io/badge/macOS%20Apple%20Silicon-000000?logo=apple&logoColor=white)
![Intel macOS](https://img.shields.io/badge/macOS%20Intel-000000?logo=apple&logoColor=white)
![ARM64 Linux](https://img.shields.io/badge/Linux%20ARM64-FCC624?logo=linux&logoColor=black)
![x64 Linux](https://img.shields.io/badge/Linux%20x64-FCC624?logo=linux&logoColor=black)
![ARM64 Windows](https://img.shields.io/badge/Windows%20ARM64-0078D6?logo=windows&logoColor=white)
![x64 Windows](https://img.shields.io/badge/Windows%20x64-0078D6?logo=windows&logoColor=white)

### Install prebuilt binaries via ...

- brew: `brew install freepicheep/tap/quiver`
- mise: `mise use -g github:freepicheep/quiver`
- shell script: `curl --proto '=https' --tlsv1.2 -LsSf https://github.com/freepicheep/quiver/releases/latest/download/quiver-installer.sh | sh`

### Build from source via Cargo

```bash
cargo install --git https://github.com/freepicheep/quiver
```

## Quick Start

```bash
# Initialize a new module project
qv init

# Or pin the Nushell version requirement up front
qv init --nu-version ">=0.109,<0.111"

# Add a dependency
qv add https://github.com/user/nu-some-module

# Or use owner/repo shorthand (defaults to github)
qv add user/nu-some-module

# Add a plugin dependency
qv add-plugin nushell/nu_plugin_inc --tag v0.91.0 --bin nu_plugin_inc

# Add a Nushell core plugin dependency by alias/name
qv add-plugin polars

# Install all dependencies from nupackage.toml
qv install

# Run a Nushell script with quiver's environment
qv run script.nu

# Activate the virtual environment
overlay use .nu-env/activate.nu

# Install all global dependencies from ~/.config/quiver/config.toml
qv install -g

# Re-resolve everything (ignore lockfile)
qv update

# Remove a dependency
qv remove nu-some-module

# List installed dependencies (project-local if nupackage.toml exists, otherwise global)
qv list

# Generate editor LSP configuration
qv lsp
```

## How It Works

A quiver project is a directory containing:

- **`nupackage.toml`** - declares package metadata and dependencies
- **`<project-dir-name>/mod.nu`** - the Nushell module entry point
- **`quiver.lock`** - auto-generated lockfile pinning exact commits (commit this to version control)

Running `qv install` (or `qv init`) sets up a `.nu-env/` virtual environment:

```
.nu-env/
├── activate.nu        # overlay that loads config.nu, exports wrapped `nu`, and `deactivate`
├── config.nu          # sets NU_LIB_DIRS/NU_PLUGIN_DIRS and exports a `nu` alias
├── plugins.msgpackz   # plugin registry/config used by nushell
├── bin/
│   ├── nu             # symlink to your system nu binary (or managed nu)
│   └── ...            # plugin binaries linked for project activation
└── modules/           # installed module dependencies
```

## Activation

Activate the virtual environment with an overlay:

```nu
overlay use .nu-env/activate.nu
```

This exports a `nu` alias that runs with `--config .nu-env/config.nu --plugin-config .nu-env/plugins.msgpackz`, so launching `nu` from the activated shell automatically includes both module and plugin paths for the project.

When you're done, run `deactivate` (or `overlay hide activate`) to unload the overlay.

You can also run project scripts directly with `qv run script.nu` (or `qv run nu script.nu`) to execute under the project config.

### Auto-activation Hook

If you want quiver projects to automatically update `$env.NU_LIB_DIRS` when you `cd` into their directory (and clean up when you leave), run:

```nu
mkdir ($nu.default-config-dir | path join "vendor" "autoload")
qv hook | save -f ($nu.default-config-dir | path join "vendor" "autoload" "quiver_hook.nu")
```

You only need to run this once. Re-run it after updating quiver to pick up any changes to the hook.

> Note: Due to Nushell's static scoping rules, the auto-activation hook only updates `$env.NU_LIB_DIRS`. For the full virtual environment experience (nu alias, deactivate), use `overlay use .nu-env/activate.nu`.

## Editor LSP Setup

Generate per-project LSP configuration so your editor's Nushell language server knows about your installed modules:

```bash
# Interactive picker — select which editors to configure
qv lsp

# Or specify editors directly
qv lsp --editor helix --editor zed
```

This generates:
- **Helix**: `.helix/languages.toml` with a `nu-lsp` language server entry
- **Zed**: `.zed/settings.json` with a `nu` language server binary config

## nupackage.toml

```toml
[package]
name = "my-module"
version = "0.1.0"
description = "a wonderful nu module anyone can use"

[dependencies.modules]
nu-utils = { git = "https://github.com/user/nu-utils", tag = "v1.0.0" }
other-lib = { git = "https://github.com/user/other-lib", branch = "main" }
pinned = { git = "https://github.com/user/pinned", rev = "a3f9c12" }

[dependencies.plugins]
nu_plugin_inc = { git = "https://github.com/nushell/nu_plugin_inc", tag = "v0.91.0", bin = "nu_plugin_inc" }
nu_plugin_polars = { source = "nu-core", bin = "nu_plugin_polars" }
```

Module dependencies must specify exactly one of `tag`, `branch`, or `rev`.
Plugin dependencies support either:
- Git source with exactly one of `tag`, `branch`, or `rev` (plus optional `bin`).
- `source = "nu-core"` with `bin` (no `git`/`tag`/`branch`/`rev`).

## Commands

| Command | Description |
|---------|-------------|
| `qv init` | Create a new `nupackage.toml`, scaffold `<project-dir-name>/mod.nu`, and set up `.nu-env/` (supports `--nu-version`) |
| `qv add <source>` | Add a module dependency from a URL or owner/repo shorthand (auto-detects latest tag) |
| `qv add-plugin <source>` | Add a plugin dependency from git or Nushell core plugin alias/name |
| `qv install` | Install dependencies from `nupackage.toml` and generate `.nu-env/` virtual environment |
| `qv install -g` | Install global dependencies from `~/.config/quiver/config.toml` |
| `qv install --frozen` | Install from lockfile only (CI-friendly) |
| `qv update` | Re-resolve all dependencies |
| `qv run <command...>` | Run a command in the current project using `.nu-env` (injects `--config` for `nu`) |
| `qv remove <name>` / `qv rm <name>` | Remove a project dependency (module or plugin) |
| `qv list` / `qv ls` | List installed dependencies (project modules/plugins or global modules) |
| `qv lsp` | Generate editor-specific LSP configuration (interactive picker) |
| `qv lsp --editor <name>` | Generate LSP config for a specific editor (helix, zed) |
| `qv version` / `qv -v` / `qv -V` / `qv --version` | Print quiver version |
| `qv hook` | Print the auto-activate hook for config.nu |

## Global config (`~/.config/quiver/config.toml`)

You can set a default git provider used for `owner/repo` shorthand in `qv add`.
Global config manages global module dependencies.

```toml
default_git_provider = "github" # default
install_mode = "clone"          # default on macOS/Linux; default is "hardlink" on Windows

# optional override
# modules_dir = "/custom/modules"

[dependencies]
nu-utils = { git = "https://github.com/user/nu-utils", tag = "v1.0.0" }

[security]
require_signed_assets = true # default
```

Supported provider aliases are `github`, `gitlab`, `codeberg`, and `bitbucket`.
You can also set a custom host like `git.example.com` or a full `https://...` base URL.

`install_mode` controls how modules are materialized into the install directory.
- `clone`: prefers copy-on-write clone behavior when available; falls back to `copy` if clone fails
- `hardlink`: uses hardlinks for files
- `copy`: always copies files

## Plugin Install Behavior

For plugin dependencies, Quiver installs in this order:
1. GitHub releases artifact matching current platform/arch (preferred).
2. Cargo build fallback from source if no usable release artifact is available.

Use `qv install --no-build-fallback` to disable source-build fallback.

After Quiver installs a plugin, make sure you enable it by running the version of nu for the package you are working in.

```nu
overlay use .nu-env/activate.nu # activates the env for this project
nu # runs the alias for the project's nu version with the modules and plugins properly pointed
plugin add <nu_plugin_name>
plugin use <name>
```

## Supply Chain Security

Quiver verifies SHA-256 checksums for downloaded release artifacts (Nushell archives and plugin release assets) before extraction or install.

- Default behavior is fail-closed: installs stop if checksum data is missing, unparseable, or mismatched.
- Quiver looks for checksum assets in the same release:
  - preferred: `SHA256SUMS` or `checksums.txt`
  - fallback: `<asset>.sha256`
- `qv install --frozen` enforces secure behavior even if local overrides are set.
- `qv install --allow-unsigned` is an explicit insecure override. Use only when you accept reduced integrity guarantees.
- If plugin release install fails and fallback remains enabled, Quiver may compile from source locally. This is a different trust boundary than verified release artifacts.

For CI, use:

```bash
qv install --frozen --no-build-fallback
```

## Disclaimer

I have not verified the security of all the code yet and I am not responsible for any grief or loss quiver may cause to you or your company.

## License

MIT
