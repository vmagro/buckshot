use std::collections::BTreeMap;

use serde::Serialize;

use super::lockfile::OptionalDep;
use super::lockfile::ResolvedPackage;

const DEFAULT_KEY: &str = "DEFAULT";

#[derive(Serialize)]
#[serde(rename = "select")]
struct Select<T>(BTreeMap<String, T>);

/// One `deps` list entry: either an unconditional target, or a flat
/// `select()` (keyed by `OptionalDep::platform`, see
/// `third-party/npm/platform/BUCK`) that resolves to the target on a
/// matching platform and `None` (dropping the entry entirely) everywhere
/// else. See `optional_dep_item`.
#[derive(Serialize)]
#[serde(untagged)]
enum DepsItem {
    Plain(String),
    Select(Select<Option<String>>),
}

/// Builds the `deps` entry for a package's own direct `optionalDependencies`
/// edge, gated on the dependency's own platform constraint (`None`
/// entirely unconditional -- shouldn't really happen for an `optional`
/// edge in practice, but falls back to an unconditional dep rather than
/// silently dropping it).
fn optional_dep_item(dep: &OptionalDep) -> DepsItem {
    let target = format!(":{}", dep.target);
    match &dep.platform {
        Some(platform) => DepsItem::Select(Select(BTreeMap::from([
            (platform.clone(), Some(target)),
            (DEFAULT_KEY.to_owned(), None),
        ]))),
        None => DepsItem::Plain(target),
    }
}

#[derive(Serialize)]
#[serde(rename = "npm_archive")]
struct NpmArchive {
    name: String,
    url: String,
    sha256: String,
    size_bytes: u64,
    // Omitted (rather than repeating `name`) whenever it matches -- true
    // for every plain, unscoped, unnested package -- since the macro
    // already defaults `package_name` to `name`.
    #[serde(skip_serializing_if = "Option::is_none")]
    package_name: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    bin: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    target_compatible_with: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    deps: Vec<DepsItem>,
}

pub fn render_buck_file(
    pkgs: &[ResolvedPackage],
    lockfile_path: &str,
    emit_tree: Option<&str>,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# @generated from {lockfile_path} by buckshot's `node npm buckify`.\n"
    ));
    out.push_str("# Do not edit by hand.\n\n");
    // Fully-qualified (not `:defs.bzl`) so the generated file works
    // verbatim in any repo with a `buckshot` cell, not just this one.
    out.push_str("load(\"@buckshot//third-party/npm:defs.bzl\", \"npm_archive\")\n");
    if emit_tree.is_some() {
        out.push_str("load(\"@buckshot//node:node_modules_tree.bzl\", \"node_modules_tree\")\n");
    }
    out.push('\n');

    let parts: Vec<String> = pkgs
        .iter()
        .map(|pkg| {
            let package_name = if pkg.package_name == pkg.target_name {
                None
            } else {
                Some(pkg.package_name.clone())
            };
            let deps = pkg
                .deps
                .iter()
                .map(|target| DepsItem::Plain(format!(":{target}")))
                .chain(pkg.optional_deps.iter().map(optional_dep_item))
                .collect();
            serde_starlark::to_string(&NpmArchive {
                name: pkg.target_name.clone(),
                url: pkg.url.clone(),
                sha256: pkg.sha256.clone(),
                size_bytes: pkg.size_bytes,
                package_name,
                bin: pkg.bin.clone(),
                target_compatible_with: pkg.compatible_with.clone(),
                deps,
            })
            .expect("NpmArchive always serializes")
        })
        .collect();
    out.push_str(&parts.join("\n"));
    if !out.ends_with('\n') {
        out.push('\n');
    }

    if let Some(tree) = emit_tree {
        // Lockfile path (leading `node_modules/` stripped, nested
        // overrides keeping their segments) -> target -- the exact shape
        // `node_modules_tree`'s `packages` dict takes.
        let mut by_relpath: Vec<(&str, &str)> = pkgs
            .iter()
            .map(|pkg| (pkg.relpath.as_str(), pkg.target_name.as_str()))
            .collect();
        by_relpath.sort();
        out.push_str("ALL_NPM_PACKAGES = {\n");
        for (relpath, target) in &by_relpath {
            out.push_str(&format!("    \"{relpath}\": \":{target}\",\n"));
        }
        out.push_str("}\n\n");
        out.push_str("node_modules_tree(\n");
        out.push_str(&format!("    name = \"{tree}\",\n"));
        out.push_str("    packages = ALL_NPM_PACKAGES,\n");
        out.push_str("    visibility = [\"PUBLIC\"],\n");
        out.push_str(")\n");
    }

    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn pkg(relpath: &str, target_name: &str, package_name: &str) -> ResolvedPackage {
        ResolvedPackage {
            relpath: relpath.to_string(),
            target_name: target_name.to_string(),
            package_name: package_name.to_string(),
            url: "https://registry.npmjs.org/x/-/x-1.0.0.tgz".to_string(),
            sha256: "abc".to_string(),
            size_bytes: 42,
            bin: BTreeMap::new(),
            compatible_with: Vec::new(),
            deps: BTreeSet::new(),
            optional_deps: Vec::new(),
        }
    }

    #[test]
    fn loads_are_fully_qualified() {
        let out = render_buck_file(&[pkg("left-pad", "left-pad", "left-pad")], "third-party/npm/package-lock.json", None);
        assert!(out.contains("load(\"@buckshot//third-party/npm:defs.bzl\", \"npm_archive\")"));
        assert!(!out.contains("load(\":defs.bzl\""));
        assert!(!out.contains("node_modules_tree"));
        assert!(out.starts_with("# @generated from third-party/npm/package-lock.json"));
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn emit_tree_adds_dict_and_target() {
        let out = render_buck_file(
            &[
                pkg("unplugin/node_modules/picomatch", "unplugin+picomatch", "picomatch"),
                pkg("left-pad", "left-pad", "left-pad"),
            ],
            "third-party/npm/package-lock.json",
            Some("node_modules"),
        );
        assert!(out.contains("load(\"@buckshot//node:node_modules_tree.bzl\", \"node_modules_tree\")"));
        assert!(out.contains("ALL_NPM_PACKAGES = {"));
        assert!(out.contains("\"left-pad\": \":left-pad\","));
        assert!(out.contains("\"unplugin/node_modules/picomatch\": \":unplugin+picomatch\","));
        assert!(out.contains("name = \"node_modules\","));
        // Relpath-sorted: nested entry sorts after the top-level one.
        let dict = &out[out.find("ALL_NPM_PACKAGES").unwrap()..];
        assert!(dict.find("left-pad").unwrap() < dict.find("unplugin").unwrap());
    }
}
