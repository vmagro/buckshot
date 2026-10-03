use anyhow::Context;
use serde::Deserialize;

#[derive(Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub assets: Vec<Asset>,
}

#[derive(Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    /// `sha256:<hex>` -- GitHub computes this itself, so there's no need to
    /// download the archive just to hash it.
    pub digest: Option<String>,
    /// Asset size in bytes, straight from the API response.
    pub size: u64,
}

/// One entry in the generated BUCK: an `http_archive` of a `install_only`
/// cpython tarball.
pub struct Component {
    pub target_name: String,
    pub url: String,
    pub sha256: String,
    // `http_archive` needs `size_bytes` alongside `sha256` or buck2
    // re-downloads the archive after every daemon restart instead of
    // recognizing the file already on disk (breaking offline builds).
    pub size_bytes: u64,
}

/// Fetches a release by tag, or -- when `tag` is `None` -- whichever release
/// GitHub currently considers `latest` (most recently published, excluding
/// drafts/prereleases).
pub async fn fetch_release(client: &reqwest::Client, tag: Option<&str>) -> anyhow::Result<Release> {
    let url = match tag {
        Some(tag) => format!(
            "https://api.github.com/repos/astral-sh/python-build-standalone/releases/tags/{tag}"
        ),
        None => "https://api.github.com/repos/astral-sh/python-build-standalone/releases/latest"
            .to_string(),
    };
    client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?
        .error_for_status()
        .with_context(|| format!("fetching {url}"))?
        .json()
        .await
        .with_context(|| format!("parsing JSON from {url}"))
}

/// A python-build-standalone release bundles many CPython versions at once
/// (3.10, 3.11, 3.12, 3.13, ...); resolve `python_version` (e.g. "3.13") to
/// the exact version present in this release (e.g. "3.13.13") by sniffing an
/// `install_only` asset name -- `cpython-<version>+<tag>-<triple>-install_only.tar.gz`.
pub fn full_python_version(
    release: &Release,
    python_version: &str,
    tag: &str,
) -> anyhow::Result<String> {
    let version_prefix = format!("{python_version}.");
    for asset in &release.assets {
        if !asset.name.ends_with("-install_only.tar.gz") || asset.name.contains("-freethreaded-") {
            continue;
        }
        let Some(rest) = asset.name.strip_prefix("cpython-") else {
            continue;
        };
        let Some((version, tail)) = rest.split_once('+') else {
            continue;
        };
        if version.starts_with(&version_prefix) && tail.starts_with(tag) {
            return Ok(version.to_string());
        }
    }
    anyhow::bail!(
        "release {tag} has no cpython-{python_version}.x install_only asset (see https://github.com/astral-sh/python-build-standalone/releases/tag/{tag})"
    )
}

pub fn select_component(
    release: &Release,
    full_version: &str,
    tag: &str,
    triple: &str,
) -> anyhow::Result<Component> {
    let name = format!("cpython-{full_version}+{tag}-{triple}-install_only.tar.gz");
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == name)
        .ok_or_else(|| {
            anyhow::anyhow!("release {tag} has no asset {name:?} (unsupported triple {triple:?}?)")
        })?;
    let digest = asset.digest.as_deref().ok_or_else(|| {
        anyhow::anyhow!("asset {name:?} has no digest in the GitHub API response")
    })?;
    let sha256 = digest
        .strip_prefix("sha256:")
        .ok_or_else(|| anyhow::anyhow!("asset {name:?} digest {digest:?} is not sha256"))?;

    Ok(Component {
        target_name: format!("cpython-{triple}"),
        url: asset.browser_download_url.clone(),
        sha256: sha256.to_string(),
        size_bytes: asset.size,
    })
}

/// python-build-standalone target triple -> (cpu, os) for the matching
/// prelude constraints. Add new mappings as triples are requested.
pub fn platform_for(triple: &str) -> anyhow::Result<(&'static str, &'static str)> {
    Ok(match triple {
        "aarch64-apple-darwin" => ("arm64", "macos"),
        "aarch64-unknown-linux-gnu" => ("arm64", "linux"),
        "x86_64-apple-darwin" => ("x86_64", "macos"),
        "x86_64-unknown-linux-gnu" => ("x86_64", "linux"),
        "x86_64-pc-windows-msvc" => ("x86_64", "windows"),
        other => anyhow::bail!(
            "unknown cpu/os mapping for triple {other:?}; add it to platform_for in buckshot/src/python/toolchain/manifest.rs"
        ),
    })
}

/// Local `config_setting` label for a (cpu, os) pair.
pub fn platform_label(cpu: &str, os_name: &str) -> String {
    format!("{os_name}-{cpu}")
}
