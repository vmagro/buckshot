//! Generates a buck2 node toolchain `BUCK` file from a nodejs.org release's
//! `SHASUMS256.txt` index.
//!
//! Reads `https://nodejs.org/dist/<version>/SHASUMS256.txt` (e.g. `version =
//! "v26.5.0"`), picks the matching archive + sha256 for every requested host
//! triple, HEADs each archive URL for its size (the index carries no sizes),
//! and writes a BUCK file whose `downloaded_node_toolchain` call
//! selects the right archive (and the right in-archive `node` bin path --
//! Windows has no `bin/` wrapper) with a flat `select()` keyed on
//! per-platform `config_setting`s, the same shape `rust/toolchain` and
//! `python/toolchain` use.
//!
//! After writing the instance it refreshes `node/toolchains/BUCK` (the
//! rolling `v<major>` aliases) from the releases on disk, so the whole
//! tree stays consistent in one run.
//!
//! Re-run any time the desired node version changes; the generated files
//! are deterministic for a given input.

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
    /// Node release version, e.g. "v26.5.0"
    /// (https://nodejs.org/dist/v26.5.0/SHASUMS256.txt).
    #[arg(long)]
    version: String,

    /// Host triple to support (repeatable). Each gets its own archive.
    #[arg(long = "host", default_values_t = default_hosts())]
    hosts: Vec<String>,

    /// Path to the BUCK file to write. Defaults to the versioned location
    /// derived from the release: `node/toolchains/<version>/BUCK`.
    #[arg(long)]
    output: Option<PathBuf>,
}

pub async fn generate(args: ToolchainArgs) -> anyhow::Result<()> {
    let shasums_url = format!("https://nodejs.org/dist/{}/SHASUMS256.txt", args.version);
    let body = reqwest::get(&shasums_url)
        .await
        .and_then(reqwest::Response::error_for_status)
        .context("fetching SHASUMS256.txt")?
        .text()
        .await
        .context("fetching SHASUMS256.txt")?;
    let shasums = manifest::parse_shasums(&body);

    let mut components = args
        .hosts
        .iter()
        .map(|triple| manifest::select_component(&shasums, &args.version, triple))
        .collect::<anyhow::Result<Vec<_>>>()?;

    // One shared client so the size HEADs reuse connections.
    let client = reqwest::Client::new();
    manifest::fetch_sizes(&client, &mut components).await?;

    let rendered = starlark::render(starlark::RenderInput {
        version: &args.version,
        host_triples: &args.hosts,
        components: &components,
    })?;

    let output = args.output.clone().unwrap_or_else(|| {
        PathBuf::from(format!("node/toolchains/{}/BUCK", args.version))
    });
    if output.ends_with("node/toolchain/BUCK") {
        anyhow::bail!(
            "refusing to write a generated instance over node/toolchain/BUCK: that path is reserved for the hand-written rule wiring shared by every release -- omit --output to write node/toolchains/{}/BUCK instead",
            args.version
        );
    }

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&output, rendered)
        .with_context(|| format!("writing {}", output.display()))?;

    eprintln!(
        "node toolchain: wrote node {} to {}",
        args.version,
        output.display()
    );

    starlark::refresh_aliases(std::path::Path::new("node/toolchains"))?;

    Ok(())
}
