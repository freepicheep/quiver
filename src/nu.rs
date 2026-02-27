use semver::{Version, VersionReq};

/// Parse a `nu-version` requirement.
///
/// Supports full semver requirement syntax. A plain version like `0.110.0`
/// is treated as an exact match requirement (`=0.110.0`).
pub fn parse_nu_version_requirement(input: &str) -> std::result::Result<VersionReq, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("cannot be empty".to_string());
    }

    if Version::parse(trimmed).is_ok() {
        let exact = format!("={trimmed}");
        return VersionReq::parse(&exact).map_err(|err| err.to_string());
    }

    VersionReq::parse(trimmed).map_err(|err| err.to_string())
}

/// Extract the first semantic version token from command output text.
pub fn extract_semver_from_text(text: &str) -> Option<Version> {
    for token in text.split_whitespace() {
        let cleaned = token
            .trim_matches(|c: char| matches!(c, ',' | ';' | '(' | ')' | '[' | ']'))
            .trim_start_matches('v');
        if let Ok(version) = Version::parse(cleaned) {
            return Some(version);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_version_as_exact_requirement() {
        let req = parse_nu_version_requirement("0.110.0").unwrap();
        assert!(req.matches(&Version::parse("0.110.0").unwrap()));
        assert!(!req.matches(&Version::parse("0.110.1").unwrap()));
    }

    #[test]
    fn parses_semver_range_requirement() {
        let req = parse_nu_version_requirement(">=0.109.0, <0.111.0").unwrap();
        assert!(req.matches(&Version::parse("0.110.2").unwrap()));
        assert!(!req.matches(&Version::parse("0.111.0").unwrap()));
    }

    #[test]
    fn extracts_semver_from_nu_version_output() {
        let version = extract_semver_from_text("nu 0.110.0 (main)").unwrap();
        assert_eq!(version, Version::parse("0.110.0").unwrap());
    }
}
