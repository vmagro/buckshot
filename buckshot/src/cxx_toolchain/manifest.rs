use anyhow::Context;
use serde::Deserialize;
use std::collections::BTreeMap;

/// One per-arch entry inside a version object:
/// `"aarch64-macos": {"tarball": ..., "shasum": ..., "size": "52238004"}`.
/// Note `size` is a JSON string, not a number.
#[derive(Deserialize)]
struct ArchEntry {
    tarball: String,
    shasum: String,
    size: String,
}

/// One entry in the generated BUCK: an `http_archive` of a Zig tarball.
pub struct Component {
    pub target_name: String,
    pub url: String,
    pub sha256: String,
    // `http_archive` needs `size_bytes` alongside `sha256` or buck2
    // re-downloads the archive after every daemon restart instead of
    // recognizing the file already on disk (breaking offline builds).
    pub size_bytes: u64,
    pub strip_prefix: String,
    /// buck2 archive `type`: `"tar.xz"` or `"zip"`.
    pub kind: String,
}

/// The whole index: version (`"0.16.0"`, plus `"master"`) -> version object.
/// Each version object mixes metadata (`version`, `date`, `notes`, `docs`,
/// `src`, `bootstrap`, ...) with per-arch entries, so it stays untyped
/// here and entries are pulled out individually in `select_component`.
pub async fn fetch_index(
    index_url: &str,
) -> anyhow::Result<BTreeMap<String, BTreeMap<String, serde_json::Value>>> {
    reqwest::get(index_url)
        .await
        .and_then(reqwest::Response::error_for_status)
        .with_context(|| format!("fetching {index_url}"))?
        .json()
        .await
        .with_context(|| format!("parsing JSON from {index_url}"))
}

fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut parts = v.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Resolve the requested version (or the latest stable when `None`,
/// skipping `"master"`), returning (version, date, version object).
pub fn select_version<'a>(
    index: &'a BTreeMap<String, BTreeMap<String, serde_json::Value>>,
    requested: Option<&str>,
) -> anyhow::Result<(String, String, &'a BTreeMap<String, serde_json::Value>)> {
    let version = match requested {
        Some(v) => v.to_string(),
        None => index
            .keys()
            .filter_map(|v| parse_version(v).map(|parsed| (parsed, v)))
            .max()
            .map(|(_, v)| v.clone())
            .ok_or_else(|| anyhow::anyhow!("index has no parseable stable versions"))?,
    };
    let obj = index
        .get(&version)
        .ok_or_else(|| anyhow::anyhow!("index has no version {version:?}"))?;
    let date = obj
        .get("date")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("index version {version:?} has no date"))?;
    Ok((version, date.to_string(), obj))
}

/// Rust-style host triple -> Zig's arch name in index.json.
pub fn zig_arch_for(triple: &str) -> anyhow::Result<&'static str> {
    Ok(match triple {
        "aarch64-apple-darwin" => "aarch64-macos",
        "x86_64-apple-darwin" => "x86_64-macos",
        "aarch64-unknown-linux-gnu" => "aarch64-linux",
        "x86_64-unknown-linux-gnu" => "x86_64-linux",
        "x86_64-pc-windows-msvc" | "x86_64-pc-windows-gnu" => "x86_64-windows",
        "aarch64-pc-windows-msvc" | "aarch64-pc-windows-gnu" => "aarch64-windows",
        other => anyhow::bail!(
            "unknown Zig arch mapping for triple {other:?}; add it to zig_arch_for in buckshot/src/cxx_toolchain/manifest.rs"
        ),
    })
}

pub fn select_component(
    obj: &BTreeMap<String, serde_json::Value>,
    version: &str,
    triple: &str,
) -> anyhow::Result<Component> {
    let arch = zig_arch_for(triple)?;
    let raw = obj
        .get(arch)
        .ok_or_else(|| anyhow::anyhow!("index version {version:?} has no {arch:?} tarball"))?;
    let entry: ArchEntry =
        serde_json::from_value(raw.clone()).with_context(|| format!("parsing {arch} entry"))?;
    let filename = entry
        .tarball
        .rsplit('/')
        .next()
        .ok_or_else(|| anyhow::anyhow!("tarball URL {:?} has no basename", entry.tarball))?;
    let (strip_prefix, kind) = match filename.strip_suffix(".tar.xz") {
        Some(stem) => (stem.to_string(), "tar.xz".to_string()),
        None => match filename.strip_suffix(".zip") {
            Some(stem) => (stem.to_string(), "zip".to_string()),
            None => anyhow::bail!("tarball {filename:?} is neither .tar.xz nor .zip"),
        },
    };
    let size_bytes: u64 = entry
        .size
        .parse()
        .with_context(|| format!("parsing size {:?} for {arch}", entry.size))?;

    Ok(Component {
        target_name: format!("zig-{triple}"),
        url: entry.tarball,
        sha256: entry.shasum,
        size_bytes,
        strip_prefix,
        kind,
    })
}

/// Rust-style triple -> (cpu, os) for the matching `platforms/configs`
/// `config_setting`. The env half (`msvc` vs `gnu`) is intentionally
/// ignored: both map to the same config label. `os` is `None` for OS-less
/// targets (wasm).
pub fn platform_for(triple: &str) -> anyhow::Result<(&'static str, Option<&'static str>)> {
    Ok(match triple {
        "aarch64-apple-darwin" => ("arm64", Some("macos")),
        "x86_64-apple-darwin" => ("x86_64", Some("macos")),
        "aarch64-unknown-linux-gnu" => ("arm64", Some("linux")),
        "x86_64-unknown-linux-gnu" => ("x86_64", Some("linux")),
        "x86_64-pc-windows-msvc" | "x86_64-pc-windows-gnu" => ("x86_64", Some("windows")),
        "aarch64-pc-windows-msvc" | "aarch64-pc-windows-gnu" => ("arm64", Some("windows")),
        "wasm32-unknown-unknown" => ("wasm32", None),
        other => anyhow::bail!(
            "unknown cpu/os mapping for triple {other:?}; add it to platform_for in buckshot/src/cxx_toolchain/manifest.rs"
        ),
    })
}

/// (cpu, os) -> the `zig -target` triple for that platform. Windows is
/// always the `-gnu` ABI: `zig c++` rejects MSVC-style flags, so the
/// toolchain links windows-gnu (see `zig_toolchain.bzl`). `unknown-unknown`
/// wasm links freestanding (same choice as `cargo-zigbuild`).
pub fn zig_target_for(cpu: &str, os_name: Option<&str>) -> anyhow::Result<&'static str> {
    Ok(match (cpu, os_name) {
        ("arm64", Some("macos")) => "aarch64-macos",
        ("x86_64", Some("macos")) => "x86_64-macos",
        ("arm64", Some("linux")) => "aarch64-linux-gnu",
        ("x86_64", Some("linux")) => "x86_64-linux-gnu",
        ("x86_64", Some("windows")) => "x86_64-windows-gnu",
        ("arm64", Some("windows")) => "aarch64-windows-gnu",
        ("wasm32", None) => "wasm32-freestanding",
        _ => anyhow::bail!("unknown Zig target for cpu {cpu:?} os {os_name:?}"),
    })
}

/// Local `config_setting` label for a (cpu, os) pair (`cpu-<cpu>` for the
/// OS-less case, e.g. wasm32).
pub fn platform_label(cpu: &str, os_name: Option<&str>) -> String {
    match os_name {
        None => format!("cpu-{cpu}"),
        Some(os_name) => format!("{os_name}-{cpu}"),
    }
}
