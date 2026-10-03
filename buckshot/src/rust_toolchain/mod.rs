//! Generates a buck2 rust toolchain `BUCK` file from a rustup release-channel
//! TOML.
//!
//! Reads the channel TOML at the given URL (e.g.
//! `https://static.rust-lang.org/dist/2026-04-27/channel-rust-nightly.toml`),
//! picks rustc / rust-std / clippy / rustfmt / cargo packages for every
//! requested host triple, plus rust-std for any cross-compile target, HEADs
//! each archive URL for its size (the TOML carries no sizes), and writes a
//! BUCK file whose `downloaded_rust_toolchain` call uses
//!
//!   - `select()` keyed on host os/cpu constraints for host-side components
//!     (rustc, clippy, rustfmt, cargo, rust_std_host) -- these resolve in
//!     the consumer's *execution* platform via `attrs.exec_dep`, so the
//!     same toolchain target works on any supported build host.
//!
//!   - `select()` keyed on target os/cpu constraints for the target rust-std
//!     + the `--target=` triple -- these resolve in the consumer's *target*
//!     platform, so a wasm32 consumer pulls in the wasm rust-std without
//!     affecting which rustc binary runs.
//!
//! Re-run any time `rust-toolchain.toml` (or the upstream channel) changes;
//! the generated file is deterministic for a given input.

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

fn default_targets() -> Vec<String> {
    [
        "wasm32-unknown-unknown",
        "aarch64-unknown-linux-gnu",
        "x86_64-unknown-linux-gnu",
        "aarch64-apple-darwin",
        "x86_64-pc-windows-msvc",
    ]
    .map(str::to_string)
    .to_vec()
}

#[derive(Args)]
pub struct ToolchainArgs {
    /// URL to a rustup channel TOML.
    #[arg(long)]
    channel_toml: String,

    /// Host triple to support (repeatable). Each gets its own
    /// rustc/rust-std/clippy/rustfmt/cargo archives.
    #[arg(long = "host", default_values_t = default_hosts())]
    hosts: Vec<String>,

    /// Cross-compile target triple (repeatable). Adds a rust-std archive for
    /// each. Hosts double as their own target -- no need to list them here.
    #[arg(long = "target", default_values_t = default_targets())]
    targets: Vec<String>,

    #[arg(long, default_value = "2024")]
    default_edition: String,

    #[arg(long)]
    no_cargo: bool,

    #[arg(long)]
    no_rustfmt: bool,

    /// Path to the BUCK file to write.
    #[arg(long, default_value = "rust/toolchain/BUCK")]
    output: PathBuf,
}

pub async fn generate(args: ToolchainArgs) -> anyhow::Result<()> {
    let body = reqwest::get(&args.channel_toml)
        .await
        .and_then(reqwest::Response::error_for_status)
        .context("fetching channel TOML")?
        .text()
        .await
        .context("fetching channel TOML")?;
    let manifest: manifest::Manifest = toml::from_str(&body).context("parsing channel TOML")?;

    let rendered = starlark::render(starlark::RenderInput {
        channel_url: &args.channel_toml,
        manifest: &manifest,
        host_triples: &args.hosts,
        extra_targets: &args.targets,
        default_edition: &args.default_edition,
        include_cargo: !args.no_cargo,
        include_rustfmt: !args.no_rustfmt,
    })
    .await?;

    if let Some(parent) = args.output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&args.output, rendered)
        .with_context(|| format!("writing {}", args.output.display()))?;

    eprintln!("rust toolchain: wrote {}", args.output.display());

    Ok(())
}
