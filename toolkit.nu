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
