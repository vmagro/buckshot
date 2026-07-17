//! Generates a buck2 python toolchain `BUCK` file from a
//! `python-build-standalone` release tag (e.g. `"20260623"`, see
//! https://github.com/astral-sh/python-build-standalone/releases).
//!
//! Fetches the release via the GitHub API -- which hands back each asset's
//! `browser_download_url` and `sha256:` digest directly, so there's no need
//! to download archives just to hash them -- picks the `install_only`
//! cpython archive for every requested host triple, and writes a BUCK file
//! whose `astral_python` call selects the right archive with a flat
//! `select()` keyed on per-platform `config_setting`s (cpu + os), the same
//! shape `rust/toolchain` uses.
//!
//! Re-run any time the desired python-build-standalone tag or version
//! changes; the generated file is deterministic for a given input.

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

    /// Path to the BUCK file to write.
    #[arg(long, default_value = "python/toolchain/BUCK")]
    output: PathBuf,
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

    if let Some(parent) = args.output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&args.output, rendered)
        .with_context(|| format!("writing {}", args.output.display()))?;

    eprintln!(
        "python toolchain: wrote cpython {full_version} (release {tag}) to {}",
        args.output.display()
    );

    Ok(())
}
