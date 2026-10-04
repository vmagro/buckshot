use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub date: String,
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
/// The TOML's `pkg.rustc.version` is e.g. `"0.98.0-nightly (...)"` for
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

/// Channel string used in tarball outer-dir/file names, derived from the
/// *release* (rustc) version -- never from each component's own version,
/// which differs on stable (cargo 0.100.0, clippy 0.1.99, rustfmt 1.10.0
/// all ship inside `*-1.99.0-*` tarballs).
pub fn release_channel_label(manifest: &Manifest) -> anyhow::Result<String> {
    let rustc = manifest
        .pkg
        .get("rustc")
        .ok_or_else(|| anyhow::anyhow!("channel manifest has no [pkg.rustc] block"))?;
    Ok(channel_label(&rustc.version))
}

pub fn select_component(
    manifest: &Manifest,
    pkg: &str,
    triple: &str,
    target_name: &str,
    channel: &str,
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

    let outer = outer_dir(pkg, channel, triple);
    let inner = component_inner_dir(pkg, triple);

    Ok(Component {
        target_name: target_name.to_string(),
        url,
        sha256,
        strip_prefix: format!("{outer}/{inner}"),
        size_bytes: None,
    })
}

/// `(channel, version)` identifying this manifest's release: dated nightlies
/// are keyed by manifest date (`nightly/2026-07-16`), stables and betas by
/// rustc version (`stable/1.99.0`). Used for the default `--output` path.
pub fn channel_and_version(manifest: &Manifest, channel_url: &str) -> anyhow::Result<(String, String)> {
    let rustc = manifest
        .pkg
        .get("rustc")
        .ok_or_else(|| anyhow::anyhow!("channel manifest has no [pkg.rustc] block"))?;
    let full = rustc.version.split(' ').next().unwrap_or(&rustc.version);
    if full.contains("-nightly") {
        if !manifest.date.is_empty() {
            return Ok(("nightly".to_string(), manifest.date.clone()));
        }
        // Fallback for manifests without a date: dated nightly channel URLs
        // embed it (.../dist/2026-07-16/channel-rust-nightly.toml).
        let date = channel_url
            .split('/')
            .find(|seg| crate::releases::is_date(seg))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "cannot determine the nightly date from the channel TOML or URL; pass --output explicitly"
                )
            })?;
        Ok(("nightly".to_string(), date.to_string()))
    } else if full.contains("-beta") {
        Ok(("beta".to_string(), full.to_string()))
    } else {
        Ok(("stable".to_string(), full.to_string()))
    }
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
        "x86_64-pc-windows-gnu" => ("x86_64", Some("windows")),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_with(date: &str, rustc_version: &str) -> Manifest {
        Manifest {
            date: date.to_string(),
            pkg: BTreeMap::from([(
                "rustc".to_string(),
                PkgBlock {
                    version: rustc_version.to_string(),
                    target: BTreeMap::new(),
                },
            )]),
        }
    }

    #[test]
    fn nightly_channel_uses_manifest_date() {
        let manifest = manifest_with("2026-07-16", "1.99.0-nightly (abc123 2026-07-15)");
        assert_eq!(
            channel_and_version(
                &manifest,
                "https://static.rust-lang.org/dist/2026-07-16/channel-rust-nightly.toml"
            )
            .unwrap(),
            ("nightly".to_string(), "2026-07-16".to_string())
        );
    }

    #[test]
    fn nightly_channel_falls_back_to_url_date() {
        let manifest = manifest_with("", "1.99.0-nightly (abc123 2026-07-15)");
        assert_eq!(
            channel_and_version(
                &manifest,
                "https://static.rust-lang.org/dist/2026-07-16/channel-rust-nightly.toml"
            )
            .unwrap(),
            ("nightly".to_string(), "2026-07-16".to_string())
        );
    }

    #[test]
    fn stable_channel_uses_rustc_version() {
        let manifest = manifest_with("2026-10-01", "1.99.0 (b940084d7 2026-09-28)");
        assert_eq!(
            channel_and_version(
                &manifest,
                "https://static.rust-lang.org/dist/channel-rust-stable.toml"
            )
            .unwrap(),
            ("stable".to_string(), "1.99.0".to_string())
        );
    }

    #[test]
    fn beta_channel_uses_rustc_version() {
        let manifest = manifest_with("2026-09-15", "1.99.0-beta.3 (abc123 2026-09-14)");
        assert_eq!(
            channel_and_version(
                &manifest,
                "https://static.rust-lang.org/dist/channel-rust-beta.toml"
            )
            .unwrap(),
            ("beta".to_string(), "1.99.0-beta.3".to_string())
        );
    }

    #[test]
    fn stable_component_dirs_use_release_version() {
        // rustfmt ships its own version (1.10.0) but inside a 1.99.0 tarball.
        fn target_block(url: &str) -> TargetBlock {
            TargetBlock {
                available: true,
                xz_url: Some(url.to_string()),
                xz_hash: Some("abc".to_string()),
            }
        }
        let manifest = Manifest {
            date: "2026-10-01".to_string(),
            pkg: BTreeMap::from([
                (
                    "rustc".to_string(),
                    PkgBlock {
                        version: "1.99.0 (b940084d7 2026-09-28)".to_string(),
                        target: BTreeMap::from([(
                            "aarch64-apple-darwin".to_string(),
                            target_block("https://example.com/rustc.tar.xz"),
                        )]),
                    },
                ),
                (
                    "rustfmt-preview".to_string(),
                    PkgBlock {
                        version: "1.10.0 (abc123 2026-01-01)".to_string(),
                        target: BTreeMap::from([(
                            "aarch64-apple-darwin".to_string(),
                            target_block("https://example.com/rustfmt.tar.xz"),
                        )]),
                    },
                ),
            ]),
        };
        let channel = release_channel_label(&manifest).unwrap();
        assert_eq!(channel, "1.99.0");
        let comp = select_component(
            &manifest,
            "rustfmt-preview",
            "aarch64-apple-darwin",
            "rustfmt-aarch64-apple-darwin",
            &channel,
        )
        .unwrap();
        assert_eq!(
            comp.strip_prefix,
            "rustfmt-1.99.0-aarch64-apple-darwin/rustfmt-preview"
        );
    }
}
