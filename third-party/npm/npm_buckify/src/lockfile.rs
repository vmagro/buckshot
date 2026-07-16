use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::Context;
use serde::Deserialize;

use crate::tarball;

#[derive(Deserialize)]
struct Lockfile {
    packages: BTreeMap<String, LockPackage>,
}

#[derive(Deserialize)]
struct LockPackage {
    version: Option<String>,
    resolved: Option<String>,
}

pub struct ResolvedPackage {
    // Lockfile path with the single leading `node_modules/` stripped.
    // Nested overrides keep their embedded `node_modules/...` segments,
    // e.g. `escodegen/node_modules/estraverse`.
    pub relpath: String,
    // Buck target name: `relpath` with every `/`-separated segment
    // (dropping literal `node_modules` segments) joined by `+`.
    pub target_name: String,
    // npm-style package name, e.g. `@babel/core`.
    pub package_name: String,
    pub url: String,
    pub sha256: String,
    pub strip_prefix: Option<String>,
    pub bin: BTreeMap<String, String>,
}

fn path_segments(relpath: &str) -> Vec<&str> {
    relpath.split('/').filter(|s| *s != "node_modules").collect()
}

fn derive_package_name(relpath: &str) -> String {
    let segs = path_segments(relpath);
    let last = segs.len() - 1;
    if last > 0 && segs[last - 1].starts_with('@') {
        format!("{}/{}", segs[last - 1], segs[last])
    } else {
        segs[last].to_string()
    }
}

fn derive_target_name(relpath: &str) -> String {
    path_segments(relpath).join("+")
}

/// npm's standard registry tarball URL layout, used when a lockfile entry
/// has no `resolved` field of its own (npm omits it for entries it considers
/// exact duplicates of content resolved elsewhere) but does have a name +
/// version.
fn default_tarball_url(name: &str, version: &str) -> String {
    let basename = name.rsplit('/').next().unwrap_or(name);
    format!("https://registry.npmjs.org/{name}/-/{basename}-{version}.tgz")
}

/// Walks every `package-lock.json` (lockfileVersion 3) entry that resolves
/// to a real registry tarball, fetches + sha256-hashes it, and returns one
/// `ResolvedPackage` per entry, keyed by its exact lockfile path.
///
/// Doesn't reimplement any of npm's hoisting resolution -- the lockfile's
/// `packages` map already encodes it as directory paths. Two shapes qualify:
///
/// - Rooted at `node_modules/...` -- the common case.
/// - Rooted at some workspace member instead, e.g.
///   `apps/foo/node_modules/@scope/bar` -- npm's own hoisting choice (some
///   packages are both a direct dep and a peerDependency with the same
///   range, and npm keeps those un-hoisted). Since a flat `node_modules_tree`
///   only has one root, any such package gets promoted there instead.
pub fn resolve_packages(
    lockfile_path: &Path,
    cache_dir: &Path,
) -> anyhow::Result<Vec<ResolvedPackage>> {
    let content = fs::read_to_string(lockfile_path)
        .with_context(|| format!("reading lockfile {}", lockfile_path.display()))?;
    let lockfile: Lockfile = serde_json::from_str(&content)
        .with_context(|| format!("parsing lockfile {}", lockfile_path.display()))?;

    let mut qualifying: Vec<(String, LockPackage)> = lockfile
        .packages
        .into_iter()
        .filter(|(key, entry)| key.contains("node_modules/") && entry.version.is_some())
        .collect();
    // Root-prefixed entries first (always authoritative), then
    // workspace-nested ones (only promoted if not already claimed).
    qualifying.sort_by_key(|(key, _)| (!key.starts_with("node_modules/"), key.clone()));
    let total = qualifying.len();

    eprintln!("npm_buckify: fetching {total} packages...");

    let mut resolved: Vec<ResolvedPackage> = Vec::new();
    let mut seen_targets: BTreeMap<String, String> = BTreeMap::new();
    let mut claimed_names: BTreeMap<String, String> = BTreeMap::new();

    for (key, entry) in qualifying {
        let is_root = key.starts_with("node_modules/");
        let relpath = if is_root {
            key["node_modules/".len()..].to_string()
        } else {
            match key.rfind("node_modules/") {
                Some(idx) => key[idx + "node_modules/".len()..].to_string(),
                None => continue,
            }
        };

        let package_name = derive_package_name(&relpath);
        if !is_root {
            if let Some(prev_key) = claimed_names.get(&package_name) {
                eprintln!("  (skipping {key}, already have {package_name} via {prev_key})");
                continue;
            }
        }
        claimed_names.insert(package_name.clone(), key.clone());

        let target_name = derive_target_name(&relpath);
        match seen_targets.get(&target_name) {
            Some(prev) if *prev != relpath => anyhow::bail!(
                "target name collision: {relpath:?} and {prev:?} both map to {target_name:?}"
            ),
            Some(_) => {}
            None => {
                seen_targets.insert(target_name.clone(), relpath.clone());
            }
        }

        let version = entry
            .version
            .as_deref()
            .expect("qualifying entries were filtered to have a version");
        let url = match &entry.resolved {
            Some(resolved_url) if resolved_url.starts_with("http") => resolved_url.clone(),
            _ => default_tarball_url(&package_name, version),
        };

        eprintln!("  [{}/{total}] {relpath}", resolved.len() + 1);
        let data = tarball::fetch_cached(&url, cache_dir)?;
        let sha256 = tarball::sha256_hex(&data);
        let info = tarball::read_tarball_info(&data)?;

        resolved.push(ResolvedPackage {
            relpath,
            target_name,
            package_name,
            url,
            sha256,
            strip_prefix: info.strip_prefix,
            bin: info.bin,
        });
    }

    Ok(resolved)
}
