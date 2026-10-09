//! Generates a buck2 node toolchain `BUCK` file from a nodejs.org release's
//! `SHASUMS256.txt` index.
//!
//! Reads `https://nodejs.org/dist/<version>/SHASUMS256.txt` (e.g. `version =
//! "v26.5.0"`), picks the matching archive + sha256 for every requested host
//! triple, HEADs each archive URL for its size (the index carries no sizes),
//! and writes a BUCK file whose `downloaded_node_toolchain` call
//! selects the right archive (and the right in-archive `node` bin path --
//! Windows has no `bin/` wrapper) with a `node_host_bundle` holding a flat
//! `select()` keyed on per-platform `config_setting`s, the same shape
//! `rust/toolchain` and `python/toolchain` use.
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

    /// TypeScript major line for the toolchain's `tsc` (e.g. `"7"`).
    /// Wires `typescript` at the rolling
    /// `node/typescript/toolchains:<major>` alias (generate it first with
    /// `node typescript`). Omit for a tsc-less toolchain.
    #[arg(long)]
    typescript_version: Option<String>,
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
        typescript_version: args.typescript_version.as_deref(),
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

#[derive(Args)]
pub struct TypescriptArgs {
    /// TypeScript release version, e.g. "7.0.2". Each host gets its own
    /// `@typescript/typescript-<platform>` native package tarball from
    /// the npm registry (self-contained: `lib/tsc` + `lib/*.d.ts`).
    #[arg(long)]
    version: String,

    /// Host triple to support (repeatable). Each gets its own archive.
    #[arg(long = "host", default_values_t = default_hosts())]
    hosts: Vec<String>,

    /// Path to the BUCK file to write. Defaults to the versioned location
    /// derived from the release: `node/typescript/toolchains/<version>/BUCK`.
    #[arg(long)]
    output: Option<PathBuf>,
}

pub async fn generate_typescript(args: TypescriptArgs) -> anyhow::Result<()> {
    // One shared client so the tarball fetches reuse connections.
    let client = reqwest::Client::new();

    let mut components = Vec::new();
    for triple in &args.hosts {
        let suffix = manifest::typescript_package(triple)?;
        let name = format!("@typescript/typescript-{suffix}");
        let url = format!("https://registry.npmjs.org/{name}/-/typescript-{suffix}-{}.tgz", args.version);
        // Same fingerprinting `node npm buckify` uses: downloads the
        // tarball for sha256/size, verifies the registry shasum, and
        // detects the wrapper dir.
        let fp = super::npm::registry::fetch_fingerprint(&client, &name, &args.version, &url).await?;
        components.push(manifest::Component {
            target_name: format!("typescript-{triple}"),
            url,
            sha256: fp.sha256,
            size_bytes: Some(fp.size_bytes),
            strip_prefix: fp.top_dir,
            kind: "tar.gz",
            bin_relpath: "lib/tsc",
        });
    }

    let rendered = starlark::render_typescript(starlark::TypescriptRenderInput {
        version: &args.version,
        host_triples: &args.hosts,
        components: &components,
    })?;

    let output = args.output.clone().unwrap_or_else(|| {
        PathBuf::from(format!("node/typescript/toolchains/{}/BUCK", args.version))
    });

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&output, rendered)
        .with_context(|| format!("writing {}", output.display()))?;

    eprintln!(
        "node typescript: wrote typescript {} to {}",
        args.version,
        output.display()
    );

    starlark::refresh_typescript_aliases(std::path::Path::new("node/typescript/toolchains"))?;

    Ok(())
}
