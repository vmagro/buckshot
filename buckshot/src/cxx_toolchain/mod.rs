//! Generates a hermetic Zig-backed cxx toolchain `BUCK` file from
//! `https://ziglang.org/download/index.json`.
//!
//! The index hands back each arch's tarball URL, `shasum`, and `size`
//! directly, so there's no need to download archives just to hash or
//! measure them. The generator picks one Zig per requested host triple and
//! writes a BUCK file with `http_archive`s, a `zig_host_bundle` (host-keyed
//! selects, like `rust/toolchain`), and one `zig_cxx_toolchain` whose
//! `target` select maps each target config to its `zig -target` triple.
//! (The `:zig_tool_wrapper` `python_bootstrap_binary` and the macOS SDK
//! shim live in the static cxx/toolchain/BUCK, shared by every release.)
//!
//! After writing the instance it refreshes `cxx/toolchains/BUCK` (the
//! rolling `<major>.<minor>` aliases) from the releases on disk, so the
//! whole tree stays consistent in one run.
//!
//! Re-run any time the desired Zig version changes; the generated files
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
    /// Zig version to use, e.g. "0.16.0" (see
    /// https://ziglang.org/download/index.json). Defaults to the latest
    /// stable version in the index.
    #[arg(long)]
    version: Option<String>,

    /// Host triple to support (repeatable). Each gets its own Zig archive.
    #[arg(long = "host", default_values_t = default_hosts())]
    hosts: Vec<String>,

    /// Target triple to support (repeatable). Each adds an arm to the
    /// toolchain's `target` select mapping its platform config to the
    /// matching `zig -target` triple. Defaults to the host triples --
    /// every host doubles as a target.
    #[arg(long = "target")]
    targets: Option<Vec<String>>,

    /// Zig download index URL.
    #[arg(long, default_value = "https://ziglang.org/download/index.json")]
    index_url: String,

    /// Path to the BUCK file to write. Defaults to the versioned location
    /// derived from the release: `cxx/toolchains/<version>/BUCK`.
    #[arg(long)]
    output: Option<PathBuf>,
}

pub async fn generate(args: ToolchainArgs) -> anyhow::Result<()> {
    let index = manifest::fetch_index(&args.index_url).await?;
    let (version, date, obj) = manifest::select_version(&index, args.version.as_deref())?;

    let targets = args.targets.unwrap_or_else(|| args.hosts.clone());
    let components = args
        .hosts
        .iter()
        .map(|triple| manifest::select_component(obj, &version, triple))
        .collect::<anyhow::Result<Vec<_>>>()?;

    let rendered = starlark::render(starlark::RenderInput {
        index_url: &args.index_url,
        version: &version,
        date: &date,
        host_triples: &args.hosts,
        components: &components,
        target_triples: &targets,
    })?;

    let output = args.output.clone().unwrap_or_else(|| {
        PathBuf::from(format!("cxx/toolchains/{version}/BUCK"))
    });
    if output.ends_with("cxx/toolchain/BUCK") {
        anyhow::bail!(
            "refusing to write a generated instance over cxx/toolchain/BUCK: that file holds the hand-written rule wiring shared by every release -- omit --output to write cxx/toolchains/{version}/BUCK instead"
        );
    }

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&output, rendered)
        .with_context(|| format!("writing {}", output.display()))?;

    eprintln!(
        "cxx toolchain: wrote zig {} ({}) to {}",
        version,
        date,
        output.display()
    );

    starlark::refresh_aliases(std::path::Path::new("cxx/toolchains"))?;

    Ok(())
}
