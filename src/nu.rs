use semver::{Comparator, Op, Version, VersionReq};

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

/// Check whether two `nu-version` requirements could ever be satisfied by the same version.
///
/// Converts each requirement to a half-open interval and tests for non-empty intersection.
/// Pre-release identifiers on the comparators are not modeled; the check operates on
/// `major.minor.patch` only, which is sufficient for the way `nu-version` is used in practice.
pub fn nu_version_reqs_compatible(req_a: &VersionReq, req_b: &VersionReq) -> bool {
    let range_a = req_to_range(req_a);
    let range_b = req_to_range(req_b);
    !range_a.intersect(&range_b).is_empty()
}

#[derive(Debug, Clone)]
struct Bound {
    version: Version,
    inclusive: bool,
}

#[derive(Debug, Clone, Default)]
struct Range {
    lower: Option<Bound>,
    upper: Option<Bound>,
}

impl Range {
    fn intersect(&self, other: &Range) -> Range {
        Range {
            lower: tighten_lower(self.lower.clone(), other.lower.clone()),
            upper: tighten_upper(self.upper.clone(), other.upper.clone()),
        }
    }

    fn is_empty(&self) -> bool {
        let (Some(low), Some(high)) = (&self.lower, &self.upper) else {
            return false;
        };
        match low.version.cmp(&high.version) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Equal => !(low.inclusive && high.inclusive),
            std::cmp::Ordering::Less => false,
        }
    }
}

fn tighten_lower(a: Option<Bound>, b: Option<Bound>) -> Option<Bound> {
    match (a, b) {
        (None, x) | (x, None) => x,
        (Some(a), Some(b)) => match a.version.cmp(&b.version) {
            std::cmp::Ordering::Less => Some(b),
            std::cmp::Ordering::Greater => Some(a),
            std::cmp::Ordering::Equal => Some(Bound {
                version: a.version,
                inclusive: a.inclusive && b.inclusive,
            }),
        },
    }
}

fn tighten_upper(a: Option<Bound>, b: Option<Bound>) -> Option<Bound> {
    match (a, b) {
        (None, x) | (x, None) => x,
        (Some(a), Some(b)) => match a.version.cmp(&b.version) {
            std::cmp::Ordering::Less => Some(a),
            std::cmp::Ordering::Greater => Some(b),
            std::cmp::Ordering::Equal => Some(Bound {
                version: a.version,
                inclusive: a.inclusive && b.inclusive,
            }),
        },
    }
}

fn req_to_range(req: &VersionReq) -> Range {
    let mut range = Range::default();
    for cmp in &req.comparators {
        range = range.intersect(&comparator_to_range(cmp));
    }
    range
}

fn inc(v: Version) -> Bound {
    Bound { version: v, inclusive: true }
}

fn exc(v: Version) -> Bound {
    Bound { version: v, inclusive: false }
}

fn comparator_to_range(c: &Comparator) -> Range {
    let major = c.major;
    let minor = c.minor;
    let patch = c.patch;

    match c.op {
        Op::Exact => match (minor, patch) {
            (Some(min), Some(pat)) => {
                let v = Version::new(major, min, pat);
                Range {
                    lower: Some(inc(v.clone())),
                    upper: Some(inc(v)),
                }
            }
            (Some(min), None) => Range {
                lower: Some(inc(Version::new(major, min, 0))),
                upper: Some(exc(Version::new(major, min + 1, 0))),
            },
            (None, _) => Range {
                lower: Some(inc(Version::new(major, 0, 0))),
                upper: Some(exc(Version::new(major + 1, 0, 0))),
            },
        },
        Op::Greater => match (minor, patch) {
            (Some(min), Some(pat)) => Range {
                lower: Some(exc(Version::new(major, min, pat))),
                upper: None,
            },
            (Some(min), None) => Range {
                lower: Some(inc(Version::new(major, min + 1, 0))),
                upper: None,
            },
            (None, _) => Range {
                lower: Some(inc(Version::new(major + 1, 0, 0))),
                upper: None,
            },
        },
        Op::GreaterEq => Range {
            lower: Some(inc(Version::new(
                major,
                minor.unwrap_or(0),
                patch.unwrap_or(0),
            ))),
            upper: None,
        },
        Op::Less => Range {
            lower: None,
            upper: Some(exc(Version::new(
                major,
                minor.unwrap_or(0),
                patch.unwrap_or(0),
            ))),
        },
        Op::LessEq => match (minor, patch) {
            (Some(min), Some(pat)) => Range {
                lower: None,
                upper: Some(inc(Version::new(major, min, pat))),
            },
            (Some(min), None) => Range {
                lower: None,
                upper: Some(exc(Version::new(major, min + 1, 0))),
            },
            (None, _) => Range {
                lower: None,
                upper: Some(exc(Version::new(major + 1, 0, 0))),
            },
        },
        Op::Tilde => match (minor, patch) {
            (Some(min), Some(pat)) => Range {
                lower: Some(inc(Version::new(major, min, pat))),
                upper: Some(exc(Version::new(major, min + 1, 0))),
            },
            (Some(min), None) => Range {
                lower: Some(inc(Version::new(major, min, 0))),
                upper: Some(exc(Version::new(major, min + 1, 0))),
            },
            (None, _) => Range {
                lower: Some(inc(Version::new(major, 0, 0))),
                upper: Some(exc(Version::new(major + 1, 0, 0))),
            },
        },
        Op::Caret => {
            let lower_v = Version::new(major, minor.unwrap_or(0), patch.unwrap_or(0));
            let upper_v = if major > 0 {
                Version::new(major + 1, 0, 0)
            } else if let Some(min) = minor {
                if min > 0 {
                    Version::new(0, min + 1, 0)
                } else if let Some(pat) = patch {
                    Version::new(0, 0, pat + 1)
                } else {
                    Version::new(0, 1, 0)
                }
            } else {
                Version::new(1, 0, 0)
            };
            Range {
                lower: Some(inc(lower_v)),
                upper: Some(exc(upper_v)),
            }
        }
        Op::Wildcard => match minor {
            Some(min) => Range {
                lower: Some(inc(Version::new(major, min, 0))),
                upper: Some(exc(Version::new(major, min + 1, 0))),
            },
            None => Range {
                lower: Some(inc(Version::new(major, 0, 0))),
                upper: Some(exc(Version::new(major + 1, 0, 0))),
            },
        },
        _ => Range::default(),
    }
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
    fn compatible_reqs_overlap() {
        let a = parse_nu_version_requirement("0.111.0").unwrap();
        let b = parse_nu_version_requirement(">=0.110.0, <0.113.0").unwrap();
        assert!(nu_version_reqs_compatible(&a, &b));
    }

    #[test]
    fn incompatible_exact_versions_do_not_overlap() {
        let a = parse_nu_version_requirement("0.111.0").unwrap();
        let b = parse_nu_version_requirement("0.110.0").unwrap();
        assert!(!nu_version_reqs_compatible(&a, &b));
    }

    #[test]
    fn non_overlapping_ranges_do_not_overlap() {
        let a = parse_nu_version_requirement(">=0.112.0").unwrap();
        let b = parse_nu_version_requirement("<0.111.0").unwrap();
        assert!(!nu_version_reqs_compatible(&a, &b));
    }

    #[test]
    fn caret_matches_patch_in_same_minor() {
        let a = parse_nu_version_requirement("^0.110.0").unwrap();
        let b = parse_nu_version_requirement("=0.110.5").unwrap();
        assert!(nu_version_reqs_compatible(&a, &b));
    }

    #[test]
    fn caret_excludes_next_minor_for_zero_major() {
        let a = parse_nu_version_requirement("^0.110.0").unwrap();
        let b = parse_nu_version_requirement("=0.111.0").unwrap();
        assert!(!nu_version_reqs_compatible(&a, &b));
    }

    #[test]
    fn tilde_matches_within_same_minor() {
        let a = parse_nu_version_requirement("~0.110.2").unwrap();
        let b = parse_nu_version_requirement("=0.110.9").unwrap();
        assert!(nu_version_reqs_compatible(&a, &b));
    }

    #[test]
    fn tilde_excludes_next_minor() {
        let a = parse_nu_version_requirement("~0.110.2").unwrap();
        let b = parse_nu_version_requirement("=0.111.0").unwrap();
        assert!(!nu_version_reqs_compatible(&a, &b));
    }

    #[test]
    fn inclusive_boundary_overlaps() {
        let a = parse_nu_version_requirement(">=0.110.0").unwrap();
        let b = parse_nu_version_requirement("<=0.110.0").unwrap();
        assert!(nu_version_reqs_compatible(&a, &b));
    }

    #[test]
    fn exclusive_boundary_does_not_overlap() {
        let a = parse_nu_version_requirement(">0.110.0").unwrap();
        let b = parse_nu_version_requirement("<=0.110.0").unwrap();
        assert!(!nu_version_reqs_compatible(&a, &b));
    }

    #[test]
    fn wildcard_matches_anything() {
        let a = parse_nu_version_requirement("*").unwrap();
        let b = parse_nu_version_requirement("0.110.0").unwrap();
        assert!(nu_version_reqs_compatible(&a, &b));
    }

    #[test]
    fn handles_versions_far_above_zero_minor_range() {
        // Regression for the previous probe-based check, which hardcoded a ceiling at 0.200.0.
        let a = parse_nu_version_requirement(">=0.500.0").unwrap();
        let b = parse_nu_version_requirement("<0.600.0").unwrap();
        assert!(nu_version_reqs_compatible(&a, &b));
    }

    #[test]
    fn handles_patch_only_requirements() {
        // Regression for the previous probe, which only sampled `0.X.0` and missed patch versions.
        let a = parse_nu_version_requirement("=0.110.5").unwrap();
        let b = parse_nu_version_requirement("=0.110.5").unwrap();
        assert!(nu_version_reqs_compatible(&a, &b));
    }

    #[test]
    fn handles_disjoint_patch_versions() {
        let a = parse_nu_version_requirement("=0.110.5").unwrap();
        let b = parse_nu_version_requirement("=0.110.6").unwrap();
        assert!(!nu_version_reqs_compatible(&a, &b));
    }

    #[test]
    fn extracts_semver_from_nu_version_output() {
        let version = extract_semver_from_text("nu 0.110.0 (main)").unwrap();
        assert_eq!(version, Version::parse("0.110.0").unwrap());
    }
}
