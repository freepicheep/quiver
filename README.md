# nuance

A module manager for [Nushell](https://www.nushell.sh/).

nuance handles dependency resolution, fetching, and lockfile management for Nushell modules distributed as git repositories.

## Install

![Apple Silicon macOS](https://img.shields.io/badge/macOS%20Apple%20Silicon-000000?logo=apple&logoColor=white)
![Intel macOS](https://img.shields.io/badge/macOS%20Intel-000000?logo=apple&logoColor=white)
![ARM64 Linux](https://img.shields.io/badge/Linux%20ARM64-FCC624?logo=linux&logoColor=black)
![x64 Linux](https://img.shields.io/badge/Linux%20x64-FCC624?logo=linux&logoColor=black)
![ARM64 Windows](https://img.shields.io/badge/Windows%20ARM64-0078D6?logo=windows&logoColor=white)
![x64 Windows](https://img.shields.io/badge/Windows%20x64-0078D6?logo=windows&logoColor=white)

### Install prebuilt binaries via ...

- brew: `brew install freepicheep/tap/nuance`
- mise: `mise use -g github:freepicheep/nuance`
- shell script: `curl --proto '=https' --tlsv1.2 -LsSf https://github.com/freepicheep/nuance/releases/latest/download/nuance-installer.sh | sh`

### Build from source via Cargo

```bash
cargo install --git https://github.com/freepicheep/nuance
```

## Quick Start

```bash
# Initialize a new module project
nuance init

# Add a dependency
nuance add https://github.com/user/nu-some-module

# Or use owner/repo shorthand (defaults to github)
nuance add user/nu-some-module

# Install all dependencies from mod.toml
nuance install

# Re-resolve everything (ignore lockfile)
nuance update

# Remove a dependency
nuance remove nu-some-module

# List installed modules (project-local if mod.toml exists, otherwise global)
nuance list
```

## How It Works

A nuance project is a directory containing:

- **`mod.toml`** — declares package metadata and dependencies
- **`mod.nu`** — the Nushell module entry point
- **`mod.lock`** — auto-generated lockfile pinning exact commits (commit this to version control)

Running `nuance install` fetches dependencies into `.nu_modules/`.

## Activation

To make the installed modules available to `use` in Nushell without specifying their full `.nu_modules/` paths, you need to add the project's modules directory to your `$env.NU_LIB_DIRS`.

Nuance provides two ways to do this:

### 1. Manual Overlay (Recommended)
`nuance install` and `nuance init` automatically generate an `activate.nu` script inside the `.nu_modules` directory. This script adds `.nu_modules/` to your `$env.NU_LIB_DIRS` **and automatically imports** all installed modules into your active scope using `export use <module> *`.

You can activate it using Nushell's `overlay` command:

```nu
overlay use .nu_modules/activate.nu
```

All commands from all installed modules are now available. When you're done, simply run `deactivate` (or `overlay hide activate`) to revert the environment changes and unload the modules.

### 2. Auto-activation Hook
If you want nuance projects to automatically update your module path when you `cd` into their directory (and remove it when you leave), add the nuance env_change hook to your `config.nu` or `env.nu`:

```bash
# Run this and append the output to your config
nuance hook
```

> **Note**: Due to Nushell's static scoping rules, the auto-activation hook can only update `$env.NU_LIB_DIRS`. It cannot automatically import the module commands (you must still type `use <module> *` interactively). For fully automatic loading, use the Manual Overlay approach above.

## mod.toml

```toml
[package]
name = "my-module"
version = "0.1.0"
description = "a wonderful nu module anyone can use"

[dependencies]
nu-utils = { git = "https://github.com/user/nu-utils", tag = "v1.0.0" }
other-lib = { git = "https://github.com/user/other-lib", branch = "main" }
pinned = { git = "https://github.com/user/pinned", rev = "a3f9c12" }
```

Each dependency must specify exactly one of `tag`, `branch`, or `rev`.

## Commands

| Command | Description |
|---------|-------------|
| `nuance init` | Create a new `mod.toml` in the current directory |
| `nuance add <source>` | Add a dependency from a URL or owner/repo shorthand (auto-detects latest tag) |
| `nuance install` | Install dependencies from `mod.toml` |
| `nuance install --frozen` | Install from lockfile only (CI-friendly) |
| `nuance update` | Re-resolve all dependencies |
| `nuance remove <name>` / `nuance rm <name>` | Remove a dependency |
| `nuance list` / `nuance ls` | List installed modules (project-local or global) |
| `nuance version` / `nuance -v` / `nuance -V` / `nuance --version` | Print nuance version |
| `nuance hook` | Print the auto-activate hook for config.nu |

## Global config (`~/.config/nuance/config.toml`)

You can set a default git provider used for `owner/repo` shorthand in `nuance add`.

```toml
default_git_provider = "github" # default
```

Supported provider aliases are `github`, `gitlab`, `codeberg`, and `bitbucket`.
You can also set a custom host like `git.example.com` or a full `https://...` base URL.

## License

MIT
