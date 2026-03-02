# AGENTS.md

## Project Overview

**quiver** is a module manager for [Nushell](https://www.nushell.sh/), written in Rust. It handles dependency resolution, fetching, and lockfile management for Nushell modules distributed as git repositories. Think of it as a minimal package manager (similar in spirit to `uv` or `cargo`) for the Nushell ecosystem.

## Architecture

The codebase is a single-crate Rust CLI application. All source lives in `src/` with one file per responsibility:

| Module | Purpose |
|---|---|
| `main.rs` | Entry point; dispatches CLI subcommands to `cmd_*` handler functions |
| `cli.rs` | CLI definition using `clap` derive macros (`Cli`, `Commands` enum) |
| `manifest.rs` | Parsing and serializing `nupackage.toml` manifests (`Manifest`, `Package`, `DependencySpec`) |
| `lockfile.rs` | Parsing and serializing `quiver.lock` lockfiles (`Lockfile`, `LockedPackage`) |
| `resolver.rs` | Dependency resolution including transitive deps and conflict detection (`ResolvedDep`) |
| `installer.rs` | Orchestrates the install/update flow: resolve → fetch → checksum → place → lock |
| `git.rs` | Git operations via `git2`: cloning, fetching, ref resolution, tree export, tag discovery |
| `checksum.rs` | Deterministic SHA-256 hashing of directory contents for lockfile integrity |
| `error.rs` | Centralized error types using `thiserror` (`NuanceError` enum, `Result<T>` alias) |

### Data flow

```
nupackage.toml → resolver (fetches git repos, resolves refs, detects conflicts)
         → installer (exports trees into .nu-env/modules/, computes checksums)
         → quiver.lock (pins exact commit SHAs and checksums)
```

### Key paths at runtime

- **Git cache**: `~/.local/share/quiver/installs/git/` — bare clones of fetched repos, keyed by URL
- **Installed modules**: `.nu-env/modules/` in the project directory
- **Manifest**: `nupackage.toml` — user-edited package metadata and dependency declarations
- **Lockfile**: `quiver.lock` — auto-generated, should be committed to version control

## Technology & Dependencies

- **Language**: Rust (edition 2024)
- **CLI framework**: `clap` v4 (derive API)
- **Git operations**: `git2` v0.20 (libgit2 bindings)
- **Serialization**: `serde` + `toml` for TOML manifest/lockfile handling
- **Hashing**: `sha2` + `hex` for SHA-256 checksums
- **Error handling**: `thiserror` for ergonomic error enums
- **File walking**: `walkdir` for recursive directory traversal
- **Distribution**: `cargo-dist` via `dist-workspace.toml`; releases target macOS, Linux, and Windows (x86_64 + aarch64)

## Conventions

### Code style
- Each source file is a single, focused module with a clear responsibility
- Public functions use `/// doc comments`; modules re-export only what is needed
- Error handling uses a project-wide `Result<T>` alias backed by the `NuanceError` enum — never use `unwrap()` in non-test code
- All user-facing output goes to stderr via `eprintln!`; stdout is reserved for structured data

### Testing
- Unit tests live in `#[cfg(test)] mod tests` blocks within each source file
- Run tests with `cargo test`
- Tests that touch the filesystem use `std::env::temp_dir()` and clean up after themselves

### Building
- `cargo build` for development
- `cargo test` to run all tests
- Release builds use the `dist` profile (`lto = "thin"`) configured in `dist-workspace.toml`

### Dependencies (nupackage.toml)
- Each dependency must specify exactly **one** of `tag`, `branch`, or `rev`
- Tags are preferred for stability; branches are resolved to a commit and locked
- Dependency conflicts (same name, different source/rev) are hard errors — no silent resolution

### File naming
- Rust source files use `snake_case.rs`
- Project files: `nupackage.toml` (manifest), `quiver.lock` (lockfile), `mod.nu` (Nushell entry point)
