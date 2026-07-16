use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use anyhow::Context;
use serde::Deserialize;

use super::tarball;

#[derive(Deserialize)]
struct Lockfile {
    packages: BTreeMap<String, LockPackage>,
}

#[derive(Deserialize)]
struct LockPackage {
    version: Option<String>,
    resolved: Option<String>,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "optionalDependencies")]
    optional_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    cpu: Vec<String>,
    #[serde(default)]
    os: Vec<String>,
}

/// npm's `os` values (`process.platform`) that have a `prelude//os:...`
/// equivalent. Anything else (`aix`, `openbsd`, `sunos`, ...) has no
/// buck2 prelude constraint to map to, so packages restricted to one of
/// those are dropped entirely rather than guessed at.
fn map_os(npm_os: &str) -> Option<&'static str> {
    Some(match npm_os {
        "darwin" => "macos",
        "linux" => "linux",
        "win32" => "windows",
        "freebsd" => "freebsd",
        "netbsd" => "netbsd",
        "android" => "android",
        _ => return None,
    })
}

/// npm's `cpu` values (`process.arch`) that have a `prelude//cpu:...`
/// equivalent. Anything else (`ppc64`, `s390x`, `mips64el`, `loong64`,
/// ...) has no buck2 prelude constraint to map to, so packages
/// restricted to one of those are dropped entirely rather than guessed at.
fn map_cpu(npm_cpu: &str) -> Option<&'static str> {
    Some(match npm_cpu {
        "x64" => "x86_64",
        "ia32" | "x86" => "x86_32",
        "arm64" => "arm64",
        "arm" => "arm32",
        "riscv64" => "riscv64",
        _ => return None,
    })
}

pub struct OptionalDep {
    pub target: String,
    pub os: Option<String>,
    pub cpu: Option<String>,
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
    // `prelude//os:...` / `prelude//cpu:...` this package is restricted
    // to, from the lockfile entry's own single-item `os`/`cpu` (see
    // `map_os`/`map_cpu` -- entries with no mappable value never make it
    // into `resolved` at all, see `resolve_packages`).
    pub os_constraint: Option<String>,
    pub cpu_constraint: Option<String>,
    // Target names of every package in this package's own transitive
    // *required* (`dependencies`, never `optionalDependencies`) closure
    // (restricted to names that also qualified as a resolved package
    // here) -- only meaningful for a package with a non-empty `bin`,
    // which is what actually needs a private `node_modules` run tree
    // assembled at execution time (see `_npm_archive`'s `deps` attr in
    // `defs.bzl`). Computed in a second pass, after every package's own
    // direct dependency names are known.
    pub deps: BTreeSet<String>,
    // This package's own *direct* `optionalDependencies` (also only
    // computed for a `bin`-having package) -- each becomes a
    // `select()`-wrapped `deps` entry keyed by the dependency's own
    // `os_constraint`/`cpu_constraint`, omitted entirely on any other
    // platform. Not expanded transitively: in practice (npm's own
    // per-platform native-binary packages) these are always leaves.
    pub optional_deps: Vec<OptionalDep>,
}

fn path_segments(relpath: &str) -> Vec<&str> {
    relpath
        .split('/')
        .filter(|s| *s != "node_modules")
        .collect()
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

    eprintln!("npm buckify: fetching {total} packages...");

    let mut resolved: Vec<ResolvedPackage> = Vec::new();
    let mut seen_targets: BTreeMap<String, String> = BTreeMap::new();
    let mut claimed_names: BTreeMap<String, String> = BTreeMap::new();
    // package_name -> (direct `dependencies` names, direct
    // `optionalDependencies` names), straight from the lockfile entry.
    // Used after the main loop to compute each bin-having package's
    // required closure + direct optional deps (see below).
    let mut direct_deps: BTreeMap<String, (Vec<String>, Vec<String>)> = BTreeMap::new();

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

        if entry.cpu.len() > 1 || entry.os.len() > 1 {
            anyhow::bail!(
                "{package_name}: multi-value `cpu`/`os` ({:?}/{:?}) isn't supported yet",
                entry.cpu,
                entry.os
            );
        }
        let os_constraint = match entry.os.first() {
            None => None,
            Some(npm_os) => match map_os(npm_os) {
                Some(mapped) => Some(format!("prelude//os:{mapped}")),
                None => {
                    eprintln!(
                        "  (skipping {package_name}, no prelude//os mapping for {npm_os:?})"
                    );
                    continue;
                }
            },
        };
        let cpu_constraint = match entry.cpu.first() {
            None => None,
            Some(npm_cpu) => match map_cpu(npm_cpu) {
                Some(mapped) => Some(format!("prelude//cpu:{mapped}")),
                None => {
                    eprintln!(
                        "  (skipping {package_name}, no prelude//cpu mapping for {npm_cpu:?})"
                    );
                    continue;
                }
            },
        };

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

        let mut required: Vec<String> = entry.dependencies.keys().cloned().collect();
        required.sort();
        required.dedup();
        let mut optional: Vec<String> = entry.optional_dependencies.keys().cloned().collect();
        optional.sort();
        optional.dedup();
        direct_deps.insert(package_name.clone(), (required, optional));

        resolved.push(ResolvedPackage {
            relpath,
            target_name,
            package_name,
            url,
            sha256,
            strip_prefix: info.strip_prefix,
            bin: info.bin,
            os_constraint,
            cpu_constraint,
            deps: BTreeSet::new(),
            optional_deps: Vec::new(),
        });
    }

    // Second pass: only a package with its own `bin` actually needs a
    // private run tree assembled at execution time (see `_npm_archive`'s
    // `deps` attr in `defs.bzl`), so only bother computing this for those
    // -- keeps the generated BUCK file's `deps` noise scoped to packages
    // that actually consume it.
    let name_to_target: BTreeMap<String, String> = resolved
        .iter()
        .map(|pkg| (pkg.package_name.clone(), pkg.target_name.clone()))
        .collect();
    let name_to_constraints: BTreeMap<String, (Option<String>, Option<String>)> = resolved
        .iter()
        .map(|pkg| {
            (
                pkg.package_name.clone(),
                (pkg.os_constraint.clone(), pkg.cpu_constraint.clone()),
            )
        })
        .collect();

    for pkg in &mut resolved {
        if pkg.bin.is_empty() {
            continue;
        }
        let Some((direct_required, direct_optional)) = direct_deps.get(&pkg.package_name) else {
            continue;
        };

        // Required closure: transitive, but only ever follows `dependencies`
        // edges -- an `optionalDependencies` edge anywhere in the chain
        // means everything past it is conditional, not unconditionally
        // required, so it's handled below instead (as a direct entry;
        // npm's own per-platform native-binary packages are always leaves,
        // so one hop is all that's needed in practice).
        let mut visited: BTreeSet<String> = BTreeSet::new();
        let mut queue: Vec<String> = direct_required.clone();
        let mut closure_targets: BTreeSet<String> = BTreeSet::new();
        while let Some(name) = queue.pop() {
            if name == pkg.package_name || !visited.insert(name.clone()) {
                continue;
            }
            if let Some(target) = name_to_target.get(&name) {
                closure_targets.insert(target.clone());
            }
            if let Some((next_required, _)) = direct_deps.get(&name) {
                queue.extend(next_required.clone());
            }
        }
        pkg.deps = closure_targets;

        pkg.optional_deps = direct_optional
            .iter()
            .filter_map(|name| {
                let target = name_to_target.get(name)?.clone();
                let (os, cpu) = name_to_constraints
                    .get(name)
                    .cloned()
                    .unwrap_or((None, None));
                Some(OptionalDep { target, os, cpu })
            })
            .collect();
    }

    Ok(resolved)
}
