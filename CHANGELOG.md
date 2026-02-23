# Unreleased

# Version 0.2.0 (2026-02-23)

## Added

- Added first-class script dependencies via `[dependencies.scripts]` with
  `nuance add-script` and `nuance remove-script`.
- Added script installation into `.nu_scripts/` from a specific path in a git
  repo or gist clone URL.
- Added lockfile artifact kinds (`module` / `script`) and script `path`
  tracking for reproducible frozen installs.

## Changed

- Switched module declarations from `[dependencies]` to
  `[dependencies.modules]`.
- Updated `activate.nu` generation and `nuance hook` output to support both
  module and script dependency paths.
- Updated module install/activation to detect real module entry paths (including
  nupm-style nested layouts) by reading `nupm.nuon` metadata hints and scanning
  for `mod.nu`, then generating `export use` statements with the discovered
  path (for example `nu-salesforce/nu-salesforce`).

# Version 0.1.1 (2026-02-21)

## Added

- Added global module management via `--global`/`-g` for `nuance install`,
  `nuance add`, and `nuance remove`.
- Added generated `.nu_modules/activate.nu` output from `nuance init` and
  `nuance install` to make project module activation easier.
- Added `nuance hook` to print a Nushell env-change hook for automatic project
  activation.
- Added configurable default git provider support for `owner/repo` shorthand in
  `nuance add` via `default_git_provider`.

## Changed

- Updated README install docs to include Homebrew, shell script, and `mise`
  installation methods.
- Updated README with badges and general formatting improvements.

# Version 0.1.0 (2026-02-20)

## Added

- Initial public release.
