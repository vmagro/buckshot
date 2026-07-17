use std::collections::BTreeMap;

/// One entry in the generated BUCK: an `http_archive` of a node release
/// archive for a single host triple.
pub struct Component {
    pub target_name: String,
    pub url: String,
    pub sha256: String,
    pub strip_prefix: String,
    /// `http_archive`'s `type` -- `"tar.xz"` on unix, `"zip"` on Windows.
    pub kind: &'static str,
    /// Path to the `node` executable inside the unpacked archive.
    pub bin_relpath: &'static str,
}

/// Parses a `SHASUMS256.txt` body (lines of `<sha256>  <filename>`) into a
/// filename -> sha256 map.
pub fn parse_shasums(body: &str) -> BTreeMap<String, String> {
    body.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let sha256 = parts.next()?;
            let filename = parts.next()?;
            Some((filename.to_string(), sha256.to_string()))
        })
        .collect()
}

/// rustup-style host triple -> (nodejs.org platform name, archive
/// extension, `node` bin path relative to the unpacked archive root).
///
/// Node's own dist archives use `<os>-<arch>` names (not triples) and
/// `.tar.xz` everywhere except Windows, whose `.zip` has no `bin/`
/// wrapper -- `node.exe` sits at the archive root.
pub fn node_platform(triple: &str) -> anyhow::Result<(&'static str, &'static str, &'static str)> {
    Ok(match triple {
        "aarch64-apple-darwin" => ("darwin-arm64", "tar.xz", "bin/node"),
        "x86_64-apple-darwin" => ("darwin-x64", "tar.xz", "bin/node"),
        "aarch64-unknown-linux-gnu" => ("linux-arm64", "tar.xz", "bin/node"),
        "x86_64-unknown-linux-gnu" => ("linux-x64", "tar.xz", "bin/node"),
        "x86_64-pc-windows-msvc" => ("win-x64", "zip", "node.exe"),
        "aarch64-pc-windows-msvc" => ("win-arm64", "zip", "node.exe"),
        other => anyhow::bail!(
            "unknown nodejs.org platform mapping for triple {other:?}; add it to node_platform in buckshot/src/node_toolchain/manifest.rs"
        ),
    })
}

/// rustup-style host triple -> (cpu, os) for the matching prelude
/// constraints -- same (cpu, os) shape/config_setting naming convention
/// `rust/toolchain` and `python/toolchain` use.
pub fn platform_for(triple: &str) -> anyhow::Result<(&'static str, &'static str)> {
    Ok(match triple {
        "aarch64-apple-darwin" => ("arm64", "macos"),
        "aarch64-unknown-linux-gnu" => ("arm64", "linux"),
        "x86_64-apple-darwin" => ("x86_64", "macos"),
        "x86_64-unknown-linux-gnu" => ("x86_64", "linux"),
        "x86_64-pc-windows-msvc" => ("x86_64", "windows"),
        "aarch64-pc-windows-msvc" => ("arm64", "windows"),
        other => anyhow::bail!(
            "unknown cpu/os mapping for triple {other:?}; add it to platform_for in buckshot/src/node_toolchain/manifest.rs"
        ),
    })
}

/// Local `config_setting` label for a (cpu, os) pair.
pub fn platform_label(cpu: &str, os_name: &str) -> String {
    format!("{os_name}-{cpu}")
}

pub fn select_component(
    shasums: &BTreeMap<String, String>,
    version: &str,
    triple: &str,
) -> anyhow::Result<Component> {
    let (platform_name, ext, bin_relpath) = node_platform(triple)?;
    let stem = format!("node-{version}-{platform_name}");
    let filename = format!("{stem}.{ext}");
    let sha256 = shasums.get(&filename).ok_or_else(|| {
        anyhow::anyhow!(
            "SHASUMS256.txt has no entry for {filename:?} (unsupported triple {triple:?} for node {version}?)"
        )
    })?;

    Ok(Component {
        target_name: format!("node-{triple}"),
        url: format!("https://nodejs.org/dist/{version}/{filename}"),
        sha256: sha256.clone(),
        strip_prefix: stem,
        kind: ext,
        bin_relpath,
    })
}
