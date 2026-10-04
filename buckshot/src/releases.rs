//! Shared helper for the toolchain generators: scanning a versioned
//! `<lang>/toolchains/` (sub)directory for releases, plus the small
//! version-identifier parsers every alias renderer needs.
//!
//! A release is a subdirectory containing a `BUCK` file; the directory name
//! is the release identifier (a nightly date, a version number -- validated
//! by each language's own alias renderer). Missing directories scan as empty
//! so a channel with no releases yet (e.g. no stable rust) needs no special
//! case, and stray files or directories without a `BUCK` file are ignored.

use std::path::Path;

use anyhow::Context;

/// Sorted names of the releases directly under `dir`.
pub fn scan_releases(dir: &Path) -> anyhow::Result<Vec<String>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut releases = Vec::new();
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("listing {}", dir.display()))?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if !entry.path().join("BUCK").is_file() {
            continue;
        }
        releases.push(entry.file_name().to_string_lossy().into_owned());
    }
    releases.sort();
    Ok(releases)
}

/// `YYYY-MM-DD` nightly-date check (lexicographic order == chronological
/// order, so plain string comparison picks the latest).
pub fn is_date(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && name
            .bytes()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
}

/// Strict `X.Y.Z` numeric version key, for release identifiers that are
/// always plain versions (rust stable, zig).
pub fn semver_key(name: &str) -> Option<(u64, u64, u64)> {
    let mut parts = name.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_key_orders_numerically() {
        assert!(semver_key("1.99.0") > semver_key("1.9.0"));
        assert!(semver_key("0.16.0") > semver_key("0.9.0"));
        assert_eq!(semver_key("1.99"), None);
        assert_eq!(semver_key("not-a-version"), None);
    }

    #[test]
    fn is_date_checks_shape() {
        assert!(is_date("2026-07-16"));
        assert!(!is_date("1.99.0"));
        assert!(!is_date("2026-7-16"));
    }
}
