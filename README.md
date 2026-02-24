<!-- LOGO -->
<h1>
<p align="center">
  <img src="https://github.com/user-attachments/assets/5bd69aeb-3c42-4a5e-8281-65b18748a43c" alt="Logo" width="128">
  <br>Quiver
</h1>
  <p align="center">
    A fast dependency manager for [Nushell](https://www.nushell.sh/).
    <br />
  </p>
</p>

Quiver handles dependency resolution, fetching, and lockfile management for Nushell modules and script dependencies distributed as git repositories.

## Install

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

# Add a dependency
qv add https://github.com/user/nu-some-module

# Or use owner/repo shorthand (defaults to github)
qv add user/nu-some-module

# Add a script dependency (single file from a repo or gist)
qv add-script user/nu-toolbox scripts/quickfix.nu

# Or pass a full blob URL directly
qv add-script https://github.com/nushell/nu_scripts/blob/main/sourced/webscraping/twitter.nu

# Install all dependencies from nupackage.toml
qv install

# Add a global script dependency (prompts for autoload placement)
qv add-script -g user/nu-toolbox scripts/quickfix.nu

# Skip the prompt and install directly into autoload
qv add-script -g --autoload user/nu-toolbox scripts/quickfix.nu

# Install all global dependencies from ~/.config/quiver/config.toml
qv install -g

# Re-resolve everything (ignore lockfile)
qv update

# Remove a dependency
qv remove nu-some-module

# Remove a script dependency
qv remove-script quickfix

# List installed dependencies (project-local if nupackage.toml exists, otherwise global)
qv list
```

## How It Works

A quiver project is a directory containing:

- **`nupackage.toml`** — declares package metadata and dependencies
- **`<project-dir-name>/mod.nu`** — the Nushell module entry point
- **`quiver.lock`** — auto-generated lockfile pinning exact commits (commit this to version control)

Running `qv install` fetches module dependencies into `.nu_modules/` and script dependencies into `.nu_scripts/`.

## Activation

To make installed modules/scripts available to `use` in Nushell without full paths, add the project's dependency directories to `$env.NU_LIB_DIRS`.

Quiver provides two ways to do this:

### 1. Manual Activation (Recommended)
`qv install` and `qv init` generate activation scripts for both dependency kinds:

- `.nu_modules/activate.nu` (module overlay): updates `$env.NU_LIB_DIRS` and imports module dependencies with `export use <name> *`
- `.nu_scripts/activate.nu` (sourceable script loader): sources script dependencies with `source <name>.nu`

Activate modules with an overlay:

```nu
overlay use .nu_modules/activate.nu
```

If you installed script dependencies, source their activate file:

```nu
source .nu_scripts/activate.nu
```

When you're done, run `deactivate` (or `overlay hide activate`) to unload the module overlay.

### 2. Auto-activation Hook

If you want quiver projects to automatically update your module path when you `cd` into their directory (and remove it when you leave), add the following to your `config.nu` or `env.nu`:

```nu
mkdir ($nu.default-config-dir | path join "vendor" "autoload")
qv hook | save -f ($nu.default-config-dir | path join "vendor" "autoload" "quiver_hook.nu")
```

You can also simply run these commands without adding them to your config or env file. You just won't receive any modifications of the hook until you run the commands again, but your shell startup time will be faster.

> **Note**: Due to Nushell's static scoping rules, the auto-activation hook only updates `$env.NU_LIB_DIRS`. It cannot auto-import module/script commands. For automatic loading, use the manual activation approach above.

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

[dependencies.scripts]
quickfix = { git = "https://github.com/user/nu-toolbox", path = "scripts/quickfix.nu", tag = "v0.4.0" }
from-gist = { git = "https://gist.github.com/<id>.git", path = "quickfix.nu", rev = "d34db33f" }
```

Module dependencies must specify exactly one of `tag`, `branch`, or `rev`.
Script dependencies must include `path` and exactly one of `tag`, `branch`, or `rev`.

## Commands

| Command | Description |
|---------|-------------|
| `qv init` | Create a new `nupackage.toml` and scaffold `<project-dir-name>/mod.nu` in the current directory |
| `qv add <source>` | Add a module dependency from a URL or owner/repo shorthand (auto-detects latest tag) |
| `qv add-script [-g] [--autoload] <source> [path]` | Add a script dependency locally (`nupackage.toml`) or globally (`config.toml`) |
| `qv install` | Install dependencies from `nupackage.toml` |
| `qv install -g` | Install global dependencies from `~/.config/quiver/config.toml` |
| `qv install --frozen` | Install from lockfile only (CI-friendly) |
| `qv update` | Re-resolve all dependencies |
| `qv remove <name>` / `qv rm <name>` | Remove a module dependency |
| `qv remove-script [-g] <name>` | Remove a script dependency locally or globally |
| `qv list` / `qv ls` | List installed dependencies (project) or modules/scripts (global) |
| `qv version` / `qv -v` / `qv -V` / `qv --version` | Print quiver version |
| `qv hook` | Print the auto-activate hook for config.nu |

## Global config (`~/.config/quiver/config.toml`)

You can set a default git provider used for `owner/repo` shorthand in `qv add`.
Global config manages global module dependencies and global script dependencies.

Global scripts install into:

- `~/.config/nushell/vendor/quiver/scripts/` (Linux)
- `~/Library/Application Support/nushell/vendor/quiver/scripts/` (macOS)

Global scripts marked for autoload install into:

- `~/.config/nushell/vendor/quiver/scripts/autoload/` (Linux)
- `~/Library/Application Support/nushell/vendor/quiver/scripts/autoload/` (macOS)

When you run `qv add-script -g`, quiver always prompts whether to install into autoload.
Pass `--autoload` to skip the prompt and install directly to autoload.

```toml
default_git_provider = "github" # default

# optional overrides
# modules_dir = "/custom/modules"
# scripts_dir = "/custom/scripts"

[dependencies]
nu-utils = { git = "https://github.com/user/nu-utils", tag = "v1.0.0" }

[scripts]
quickfix = { git = "https://github.com/user/nu-toolbox", path = "scripts/quickfix.nu", tag = "v0.4.0", autoload = true }
```

Supported provider aliases are `github`, `gitlab`, `codeberg`, and `bitbucket`.
You can also set a custom host like `git.example.com` or a full `https://...` base URL.

## License

MIT
