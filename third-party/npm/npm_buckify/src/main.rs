//! Generates `third-party/npm/BUCK` from `third-party/npm/package-lock.json`
//! (lockfileVersion 3) -- the npm equivalent of `reindeer buckify` for Rust
//! crates. See `third-party/npm/README.md` for the full picture; in short,
//! every resolved third-party package gets its own `http_archive` +
//! `npm_archive` pair, keyed by its exact lockfile path, and nothing else --
//! no aggregate target pulls in the whole third-party set.
//!
//! Usage:
//!
//!     buck2 run third-party/npm/npm_buckify:npm_buckify
//!
//! Re-run any time the lockfile changes; the generated file is deterministic
//! for a given input. Downloaded tarballs are cached under
//! `.npm_buckify_cache/` across runs (override with `--cache-dir`).

mod lockfile;
mod starlark;
mod tarball;

use std::path::PathBuf;

use anyhow::Context;

struct Args {
    lockfile: PathBuf,
    out_dir: PathBuf,
    cache_dir: PathBuf,
}

fn parse_args() -> Args {
    let mut lockfile = PathBuf::from("third-party/npm/package-lock.json");
    let mut out_dir = PathBuf::from("third-party/npm");
    let mut cache_dir = PathBuf::from(".npm_buckify_cache");

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value_for = |flag: &str| -> String {
            args.next()
                .unwrap_or_else(|| panic!("{flag} requires a value"))
        };
        match arg.as_str() {
            "--lockfile" => lockfile = PathBuf::from(value_for("--lockfile")),
            "--out-dir" => out_dir = PathBuf::from(value_for("--out-dir")),
            "--cache-dir" => cache_dir = PathBuf::from(value_for("--cache-dir")),
            other => panic!("unrecognized argument: {other}"),
        }
    }

    Args { lockfile, out_dir, cache_dir }
}

fn main() -> anyhow::Result<()> {
    let args = parse_args();

    std::fs::create_dir_all(&args.cache_dir)
        .with_context(|| format!("creating cache dir {}", args.cache_dir.display()))?;

    let resolved = lockfile::resolve_packages(&args.lockfile, &args.cache_dir)?;
    let buck_file = starlark::render_buck_file(&resolved, &args.lockfile.display().to_string());

    std::fs::create_dir_all(&args.out_dir)
        .with_context(|| format!("creating out dir {}", args.out_dir.display()))?;
    let out_path = args.out_dir.join("BUCK");
    std::fs::write(&out_path, buck_file)
        .with_context(|| format!("writing {}", out_path.display()))?;

    eprintln!(
        "npm_buckify: wrote {} packages to {}",
        resolved.len(),
        out_path.display()
    );

    Ok(())
}
