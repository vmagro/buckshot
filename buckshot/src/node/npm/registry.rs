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

/// Fetches `dist.shasum` for one exact registry version straight from
/// `https://registry.npmjs.org/<name>/<version>` -- the registry already
/// computed this, so there's no need to download and hash the tarball
/// ourselves just to get its checksum.
pub async fn fetch_shasum(
    client: &reqwest::Client,
    name: &str,
    version: &str,
) -> anyhow::Result<String> {
    let url = format!("https://registry.npmjs.org/{name}/{version}");
    let metadata: VersionMetadata = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?
        .error_for_status()
        .with_context(|| format!("fetching {url}"))?
        .json()
        .await
        .with_context(|| format!("parsing JSON from {url}"))?;
    Ok(metadata.dist.shasum)
}
