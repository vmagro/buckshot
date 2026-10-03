use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Deserialize)]
pub struct Manifest {
    pub pkg: BTreeMap<String, PkgBlock>,
}

#[derive(Deserialize)]
pub struct PkgBlock {
    #[serde(default)]
    pub version: String,
    pub target: BTreeMap<String, TargetBlock>,
}

#[derive(Deserialize)]
pub struct TargetBlock {
    #[serde(default)]
    pub available: bool,
    pub xz_url: Option<String>,
    pub xz_hash: Option<String>,
}

/// One entry in the generated BUCK: an `http_archive` of a component tarball.
pub struct Component {
    pub target_name: String,
    pub url: String,
    pub sha256: String,
    // `<outer>/<inner>` to drop the wrapper dirs.
    pub strip_prefix: String,
    // Archive size in bytes, HEADed from `url` after selection (see
    // `fetch_sizes`): `http_archive` needs `size_bytes` alongside `sha256`
    // or buck2 re-downloads the archive after every daemon restart instead
    // of recognizing the file already on disk (breaking offline builds).
    pub size_bytes: Option<u64>,
}

/// Inner directory inside a rustup component tarball.
///
/// Tarballs unpack to `<outer>/<inner>/...`. The inner dir name matches the
/// manifest's package id (including any `-preview` suffix) for most
/// components. `rust-std` is the exception: its inner dir is
/// `rust-std-<triple>`.
fn component_inner_dir(pkg: &str, triple: &str) -> String {
    if pkg == "rust-std" {
        format!("rust-std-{triple}")
    } else {
        pkg.to_string()
    }
}

/// Top-level directory inside the tarball.
///
/// The outer dir uses the URL package name (without `-preview`) -- e.g.
/// `clippy-nightly-<triple>`, not `clippy-preview-nightly-<triple>`.
fn outer_dir(pkg: &str, channel: &str, triple: &str) -> String {
    let short = pkg.strip_suffix("-preview").unwrap_or(pkg);
    format!("{short}-{channel}-{triple}")
}

/// Best-effort channel string used in tarball filenames.
///
/// Nightly tarballs are `<pkg>-nightly-<triple>.tar.xz`, stable are
/// `<pkg>-<version>-<triple>.tar.xz`, beta are `<pkg>-beta-<triple>.tar.xz`.
/// The TOML's `pkg.<X>.version` is e.g. `"0.98.0-nightly (...)"` for
/// nightly; sniff the suffix.
fn channel_label(version: &str) -> String {
    let suffix = version.split(' ').next().unwrap_or(version);
    if suffix.contains("-nightly") {
        "nightly".to_string()
    } else if suffix.contains("-beta") {
        "beta".to_string()
    } else {
        suffix.to_string()
    }
}

pub fn select_component(
    manifest: &Manifest,
    pkg: &str,
    triple: &str,
    target_name: &str,
) -> anyhow::Result<Component> {
    let pkg_block = manifest
        .pkg
        .get(pkg)
        .ok_or_else(|| anyhow::anyhow!("channel manifest has no [pkg.{pkg}] block"))?;

    let target_block = pkg_block.target.get(triple).ok_or_else(|| {
        anyhow::anyhow!(
            "[pkg.{pkg}.target.{triple}] not present in manifest (typo? unsupported target?)"
        )
    })?;
    if !target_block.available {
        anyhow::bail!("[pkg.{pkg}.target.{triple}] is not `available = true`");
    }

    let url = target_block
        .xz_url
        .clone()
        .ok_or_else(|| anyhow::anyhow!("[pkg.{pkg}.target.{triple}] missing xz_url"))?;
    let sha256 = target_block
        .xz_hash
        .clone()
        .ok_or_else(|| anyhow::anyhow!("[pkg.{pkg}.target.{triple}] missing xz_hash"))?;

    let channel = channel_label(&pkg_block.version);
    let outer = outer_dir(pkg, &channel, triple);
    let inner = component_inner_dir(pkg, triple);

    Ok(Component {
        target_name: target_name.to_string(),
        url,
        sha256,
        strip_prefix: format!("{outer}/{inner}"),
        size_bytes: None,
    })
}

/// rustup target triple -> (cpu, os) for the matching prelude constraints.
/// `os` is `None` for OS-less targets (wasm). Add new mappings as targets
/// are requested.
pub fn platform_for(triple: &str) -> anyhow::Result<(&'static str, Option<&'static str>)> {
    Ok(match triple {
        "aarch64-apple-darwin" => ("arm64", Some("macos")),
        "aarch64-unknown-linux-gnu" => ("arm64", Some("linux")),
        "x86_64-apple-darwin" => ("x86_64", Some("macos")),
        "x86_64-unknown-linux-gnu" => ("x86_64", Some("linux")),
        "x86_64-pc-windows-msvc" => ("x86_64", Some("windows")),
        "wasm32-unknown-unknown" => ("wasm32", None),
        "wasm32-wasip1" => ("wasm32", Some("wasi")),
        other => anyhow::bail!(
            "unknown cpu/os mapping for triple {other:?}; add it to platform_for in buckshot/src/rust_toolchain/manifest.rs"
        ),
    })
}

/// Local `config_setting` label for a (cpu, os) pair.
pub fn platform_label(cpu: &str, os_name: Option<&str>) -> String {
    match os_name {
        // wasm32-unknown-unknown -- cpu alone disambiguates.
        None => format!("cpu-{cpu}"),
        Some(os_name) => format!("{os_name}-{cpu}"),
    }
}
