//! Generates `third-party/npm/BUCK` from `third-party/npm/package-lock.json`
//! (lockfileVersion 3) -- the npm equivalent of `reindeer buckify` for Rust
//! crates. See `third-party/npm/README.md` for the full picture; in short,
//! every resolved third-party package gets its own `npm_archive` macro call,
//! keyed by its exact lockfile path, and nothing else -- no aggregate target
//! pulls in the whole third-party set.

mod app_deps;
mod lockfile;
// `pub(crate)`: the `node typescript` generator reuses tarball
// fingerprinting for the TypeScript native packages.
pub(crate) mod registry;
mod starlark;

pub(crate) use app_deps::AppDepsArgs;

use std::path::PathBuf;

use anyhow::Context;
use clap::Args;
use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum NpmCommand {
    /// Generate third-party/npm/BUCK from package-lock.json
    Buckify(BuckifyArgs),
    /// Print the vite_bundle `deps` block for roots + transitive closure
    AppDeps(AppDepsArgs),
}

#[derive(Args)]
pub struct BuckifyArgs {
    /// Path to package-lock.json (lockfileVersion 3).
    #[arg(long, default_value = "third-party/npm/package-lock.json")]
    lockfile: PathBuf,

    /// Directory to write the generated BUCK file into.
    #[arg(long, default_value = "third-party/npm")]
    out_dir: PathBuf,

    /// Also emit an `ALL_NPM_PACKAGES` dict (lockfile path -> target) plus
    /// a `node_modules_tree` target of this name holding every package --
    /// the prebuilt third-party tree bundles layer internal packages onto.
    #[arg(long)]
    emit_tree: Option<String>,

    /// Vendor every platform-specific leaf unconditionally (a plain `deps`
    /// entry plus no `target_compatible_with`) instead of `select()`-gating
    /// it on its own platform.
    ///
    /// Needed when bundles build under a non-host target config (e.g.
    /// behind a wasm32 transition) while vite itself still executes on the
    /// host: selects see the target config, so every platform leaf would
    /// resolve to `None` and go missing from the staged tree.
    #[arg(long)]
    all_platforms: bool,
}

pub async fn buckify(args: BuckifyArgs) -> anyhow::Result<()> {
    // Reused across every package's registry-metadata fetch, so connections
    // to registry.npmjs.org get pooled instead of each request reconnecting
    // from scratch.
    let client = reqwest::Client::new();
    let resolved = lockfile::resolve_packages(&client, &args.lockfile).await?;
    let buck_file = starlark::render_buck_file(
        &resolved,
        &args.lockfile.display().to_string(),
        args.emit_tree.as_deref(),
        args.all_platforms,
    );

    std::fs::create_dir_all(&args.out_dir)
        .with_context(|| format!("creating out dir {}", args.out_dir.display()))?;
    let out_path = args.out_dir.join("BUCK");
    std::fs::write(&out_path, buck_file)
        .with_context(|| format!("writing {}", out_path.display()))?;

    eprintln!(
        "node npm buckify: wrote {} packages to {}",
        resolved.len(),
        out_path.display()
    );

    Ok(())
}

impl NpmCommand {
    pub(crate) async fn run(self) -> anyhow::Result<()> {
        match self {
            NpmCommand::Buckify(args) => buckify(args).await,
            NpmCommand::AppDeps(args) => app_deps::print_deps(args),
        }
    }
}
