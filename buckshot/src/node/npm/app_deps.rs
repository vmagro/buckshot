//! Prints a `vite_bundle` `deps` block: every vendored target in the
//! transitive *required* (`dependencies`) closure of the given root
//! package names, plus nested overrides of those same packages.
//!
//! `vite_bundle` flattens each dep's own `JsPackageInfo.node_modules`
//! dict, and `buckify` computes the full required closure (plus
//! select-gated platform optionals) for every `bin`-having package --
//! so roots plus their required transitive closure is exactly the
//! explicit list a bundle needs. No network: versions/URLs are never
//! touched, only the lockfile's already-resolved dependency edges.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::PathBuf;

use clap::Args;

use super::lockfile;

#[derive(Args)]
pub struct AppDepsArgs {
    /// Path to package-lock.json (lockfileVersion 3).
    #[arg(long, default_value = "third-party/npm/package-lock.json")]
    lockfile: PathBuf,

    /// Root package names (npm-style, comma-separated), e.g.
    /// `react,react-dom,vite,@vitejs/plugin-react`.
    #[arg(long, value_delimiter = ',')]
    roots: Vec<String>,

    /// Label prefix for the vendored targets, e.g. `third-party//npm`
    /// (defaults to this repo's own layout).
    #[arg(long, default_value = "buckshot//third-party/npm")]
    prefix: String,
}

fn closure_targets(entries: &[lockfile::LockEntry], roots: &[String]) -> (BTreeSet<String>, usize) {
    let mut edges: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for entry in entries {
        edges
            .entry(entry.package_name.as_str())
            .or_default()
            .extend(entry.dependencies.iter().map(String::as_str));
    }

    let mut closure: BTreeSet<&str> = BTreeSet::new();
    let mut queue: Vec<&str> = roots.iter().map(String::as_str).collect();
    while let Some(name) = queue.pop() {
        if !closure.insert(name) {
            continue;
        }
        if let Some(deps) = edges.get(name) {
            queue.extend(deps.iter().copied());
        }
    }

    let mut targets: BTreeSet<String> = BTreeSet::new();
    for entry in entries {
        if closure.contains(entry.package_name.as_str()) {
            targets.insert(entry.target_name.clone());
        }
    }
    (targets, closure.len())
}

pub fn print_deps(args: AppDepsArgs) -> anyhow::Result<()> {
    let entries = lockfile::read_entries(&args.lockfile)?;

    for root in &args.roots {
        if !entries.iter().any(|e| e.package_name == *root) {
            anyhow::bail!("root `{root}` not found in {}", args.lockfile.display());
        }
    }

    let (targets, closure_len) = closure_targets(&entries, &args.roots);

    println!("    deps = [");
    for target in &targets {
        println!("        \"{}:{}\",", args.prefix, target);
    }
    println!("    ],");
    eprintln!(
        "node npm app-deps: {} targets, {} packages in closure",
        targets.len(),
        closure_len
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(relpath: &str, target: &str, name: &str, deps: &[&str]) -> lockfile::LockEntry {
        lockfile::LockEntry {
            relpath: relpath.to_string(),
            target_name: target.to_string(),
            package_name: name.to_string(),
            dependencies: deps.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn walks_required_closure_and_keeps_nested_overrides() {
        let entries = vec![
            entry("a", "a", "a", &["b"]),
            entry("b", "b", "b", &["c"]),
            entry("c", "c", "c", &[]),
            entry("a/node_modules/c", "a+c", "c", &[]),
            entry("unrelated", "unrelated", "unrelated", &[]),
        ];
        let (targets, len) = closure_targets(&entries, &["a".to_string()]);
        assert_eq!(len, 3);
        assert_eq!(
            targets,
            BTreeSet::from(["a".to_string(), "b".to_string(), "c".to_string(), "a+c".to_string()])
        );
    }
}
