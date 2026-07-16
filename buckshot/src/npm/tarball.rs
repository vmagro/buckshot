use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Read;
use std::path::Path;

use anyhow::Context;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::http;

pub struct TarballInfo {
    pub strip_prefix: Option<String>,
    pub bin: BTreeMap<String, String>,
}

pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(out, "{byte:02x}").unwrap();
    }
    out
}

pub fn fetch_cached(url: &str, cache_dir: &Path) -> anyhow::Result<Vec<u8>> {
    let cache_key = sha256_hex(url.as_bytes());
    let cache_path = cache_dir.join(format!("{cache_key}.tgz"));
    if cache_path.exists() {
        return std::fs::read(&cache_path)
            .with_context(|| format!("reading cached {}", cache_path.display()));
    }
    let data = http::fetch_bytes(url)?;
    std::fs::write(&cache_path, &data)
        .with_context(|| format!("writing cache {}", cache_path.display()))?;
    Ok(data)
}

#[derive(Deserialize)]
struct PackageJson {
    name: Option<String>,
    #[serde(default)]
    bin: Option<BinField>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum BinField {
    Single(String),
    Map(BTreeMap<String, serde_json::Value>),
}

fn extract_bin(pkg: PackageJson) -> BTreeMap<String, String> {
    match pkg.bin {
        Some(BinField::Single(path)) => {
            let name = pkg.name.unwrap_or_default();
            let bare = name.rsplit('/').next().unwrap_or_default().to_string();
            BTreeMap::from([(bare, path)])
        }
        Some(BinField::Map(map)) => map
            .into_iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
            .collect(),
        None => BTreeMap::new(),
    }
}

/// Finds the tarball's top-level `package.json` (whatever the top-level
/// directory is actually named -- `npm pack` conventionally uses `package/`,
/// but that's not universal, e.g. `@types/node`'s tarball uses `node v22.19`,
/// spaces included) and returns its normalized `bin` field alongside the
/// discovered directory name to use as this package's `http_archive`
/// `strip_prefix`.
pub fn read_tarball_info(tarball: &[u8]) -> anyhow::Result<TarballInfo> {
    let gz = flate2::read::GzDecoder::new(tarball);
    let mut archive = tar::Archive::new(gz);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = entry.path()?.to_string_lossy().into_owned();
        let comps: Vec<&str> = name.split('/').filter(|c| !c.is_empty()).collect();
        let strip_prefix = if comps.len() == 2 && comps[1] == "package.json" {
            Some(comps[0].to_string())
        } else if comps.len() == 1 && comps[0] == "package.json" {
            None
        } else {
            continue;
        };
        let mut buf = String::new();
        entry.read_to_string(&mut buf)?;
        let pkg: PackageJson =
            serde_json::from_str(&buf).context("parsing tarball package.json")?;
        return Ok(TarballInfo { strip_prefix, bin: extract_bin(pkg) });
    }
    anyhow::bail!("no top-level package.json entry found in tarball")
}
