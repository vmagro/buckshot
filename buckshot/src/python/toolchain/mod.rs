//! Generates a buck2 python toolchain `BUCK` file from a
//! `python-build-standalone` release tag (e.g. `"20260623"`, see
//! https://github.com/astral-sh/python-build-standalone/releases).
//!
//! Fetches the release via the GitHub API -- which hands back each asset's
//! `browser_download_url`, `sha256:` digest, and `size` directly, so there's
//! no need to download archives just to hash or measure them -- picks the `install_only`
//! cpython archive for every requested host triple, and writes a BUCK file
//! whose `astral_python` call selects the right archive with a flat
//! `select()` keyed on per-platform `config_setting`s (cpu + os), the same
//! shape `rust/toolchain` uses.
//!
//! After writing the instance it refreshes `python/toolchains/BUCK` (the
//! rolling `<major>.<minor>` aliases) from the releases on disk, so the
//! whole tree stays consistent in one run.
//!
//! Re-run any time the desired python-build-standalone tag or version
//! changes; the generated files are deterministic for a given input.

mod manifest;
mod starlark;

use std::path::PathBuf;

use anyhow::Context;
use clap::Args;

fn default_hosts() -> Vec<String> {
    [
        "aarch64-apple-darwin",
        "aarch64-unknown-linux-gnu",
        "x86_64-unknown-linux-gnu",
        "x86_64-pc-windows-msvc",
    ]
    .map(str::to_string)
    .to_vec()
}

#[derive(Args)]
pub struct ToolchainArgs {
    /// python-build-standalone release tag, e.g. "20260623"
    /// (https://github.com/astral-sh/python-build-standalone/releases/tag/20260623).
    /// Defaults to whichever release GitHub currently considers `latest`.
    #[arg(long)]
    tag: Option<String>,

    /// CPython major.minor version to select from the release -- a single
    /// release tag bundles many CPython versions.
    #[arg(long)]
    python_version: String,

    /// Host triple to support (repeatable). Each gets its own
    /// `install_only` archive.
    #[arg(long = "host", default_values_t = default_hosts())]
    hosts: Vec<String>,

    /// Path to the BUCK file to write. Defaults to the versioned location
    /// derived from the release: `python/toolchains/<version>/BUCK`.
    #[arg(long)]
    output: Option<PathBuf>,
}

pub async fn generate(args: ToolchainArgs) -> anyhow::Result<()> {
    // GitHub's API requires a User-Agent header on every request.
    let client = reqwest::Client::builder()
        .user_agent("buckshot")
        .build()
        .context("building HTTP client")?;

    let release = manifest::fetch_release(&client, args.tag.as_deref()).await?;
    let tag = release.tag_name.clone();
    let full_version = manifest::full_python_version(&release, &args.python_version, &tag)?;

    let components = args
        .hosts
        .iter()
        .map(|triple| manifest::select_component(&release, &full_version, &tag, triple))
        .collect::<anyhow::Result<Vec<_>>>()?;

    let rendered = starlark::render(starlark::RenderInput {
        tag: &tag,
        full_version: &full_version,
        host_triples: &args.hosts,
        components: &components,
    })?;

    let output = args.output.clone().unwrap_or_else(|| {
        PathBuf::from(format!("python/toolchains/{full_version}/BUCK"))
    });
    if output.ends_with("python/toolchain/BUCK") {
        anyhow::bail!(
            "refusing to write a generated instance over python/toolchain/BUCK: that path is reserved for the hand-written rule wiring shared by every release -- omit --output to write python/toolchains/{full_version}/BUCK instead"
        );
    }

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&output, rendered)
        .with_context(|| format!("writing {}", output.display()))?;

    eprintln!(
        "python toolchain: wrote cpython {full_version} (release {tag}) to {}",
        output.display()
    );

    starlark::refresh_aliases(std::path::Path::new("python/toolchains"))?;

    Ok(())
}
