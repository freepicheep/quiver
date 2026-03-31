# Release a new version based on package version in Cargo.toml
@example "release new version based on Cargo.toml" { release }
export def release [] {
    let version = open Cargo.toml | get package.version
    let confirm = input ($"> Package version is ($version). Create new tag for this version and push to GitHub? \(y/n\) ")
    if (($confirm | str downcase) == 'y') {
        git tag $"v($version)"
        git push origin $"v($version)"
    } else {
        print "Cancelled tagged release."
    }
}

# Bump the package's minor version
@example "specify a new version" { bump-cargo-version 1.0.0 } --result $"Quiver updated from (ansi yellow)0.3.4(ansi reset) to (ansi green)1.0.0"
@example "bump the minor version" { bump-cargo-version } --result $"Quiver updated from (ansi yellow)0.3.4(ansi reset) to (ansi green)0.4.0"
export def bump-cargo-version [version?: string] {
    let current_version = open Cargo.toml | get package.version
    mut new_version = $version | default ($current_version | inc -m)
    open Cargo.toml | update package.version $new_version | collect | save -f Cargo.toml
    print $"Quiver updated from (ansi yellow)($current_version)(ansi reset) to (ansi green)($new_version)"
}

# Alias to bump package's minor version
export alias bcv = bump-cargo-version
