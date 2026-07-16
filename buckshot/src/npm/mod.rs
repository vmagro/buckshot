//! Generates `third-party/npm/BUCK` from `third-party/npm/package-lock.json`
//! (lockfileVersion 3) -- the npm equivalent of `reindeer buckify` for Rust
//! crates. See `third-party/npm/README.md` for the full picture; in short,
//! every resolved third-party package gets its own `npm_archive` macro call,
//! keyed by its exact lockfile path, and nothing else -- no aggregate target
//! pulls in the whole third-party set.

mod lockfile;
mod starlark;
mod tarball;

use std::path::PathBuf;

use anyhow::Context;
use clap::Args;

#[derive(Args)]
pub struct BuckifyArgs {
    /// Path to package-lock.json (lockfileVersion 3).
    #[arg(long, default_value = "third-party/npm/package-lock.json")]
    lockfile: PathBuf,

    /// Directory to write the generated BUCK file into.
    #[arg(long, default_value = "third-party/npm")]
    out_dir: PathBuf,

    /// Directory to cache downloaded tarballs in across runs.
    #[arg(long, default_value = ".npm_buckify_cache")]
    cache_dir: PathBuf,
}

pub fn buckify(args: BuckifyArgs) -> anyhow::Result<()> {
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
        "npm buckify: wrote {} packages to {}",
        resolved.len(),
        out_path.display()
    );

    Ok(())
}
