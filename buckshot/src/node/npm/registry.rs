use anyhow::Context;
use serde::Deserialize;

#[derive(Deserialize)]
struct VersionMetadata {
    dist: Dist,
}

#[derive(Deserialize)]
struct Dist {
    shasum: String,
}

/// What the generated `npm_archive` call pins: the tarball's sha256 and
/// byte size. `http_archive` needs both or buck2 re-downloads the tarball
/// after every daemon restart instead of recognizing the file already on
/// disk (breaking offline builds) -- and it only accepts sha1/sha256, so
/// npm's own sha512 `integrity` can't be used directly.
pub struct Fingerprint {
    pub sha256: String,
    pub size_bytes: u64,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Fingerprints one exact registry version: downloads the tarball itself,
/// hashing sha256 + counting bytes, and checks the simultaneously-computed
/// sha1 against the registry's own recorded `dist.shasum` for that
/// version. The download is the only source of sha256/size (the registry
/// API reports neither), and the shasum check guards against a corrupt or
/// tampered fetch.
pub async fn fetch_fingerprint(
    client: &reqwest::Client,
    name: &str,
    version: &str,
    url: &str,
) -> anyhow::Result<Fingerprint> {
    let meta_url = format!("https://registry.npmjs.org/{name}/{version}");
    let metadata: VersionMetadata = client
        .get(&meta_url)
        .send()
        .await
        .with_context(|| format!("fetching {meta_url}"))?
        .error_for_status()
        .with_context(|| format!("fetching {meta_url}"))?
        .json()
        .await
        .with_context(|| format!("parsing JSON from {meta_url}"))?;

    let bytes = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?
        .error_for_status()
        .with_context(|| format!("fetching {url}"))?
        .bytes()
        .await
        .with_context(|| format!("reading {url}"))?;

    let mut sha1_ctx = ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY);
    sha1_ctx.update(&bytes);
    let got_sha1 = hex(sha1_ctx.finish().as_ref());
    if got_sha1 != metadata.dist.shasum {
        anyhow::bail!(
            "{url}: downloaded sha1 {got_sha1} != registry dist.shasum {}",
            metadata.dist.shasum
        );
    }

    let mut sha256_ctx = ring::digest::Context::new(&ring::digest::SHA256);
    sha256_ctx.update(&bytes);
    Ok(Fingerprint {
        sha256: hex(sha256_ctx.finish().as_ref()),
        size_bytes: bytes.len() as u64,
    })
}
