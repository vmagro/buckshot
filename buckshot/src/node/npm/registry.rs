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
    /// Top-level wrapper directory (`package` by npm-pack convention,
    /// but real tarballs vary -- `@types/estree` uses `estree/`).
    pub top_dir: String,
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
    let top_dir = wrapper_dir(&url, &list_tarball(&url, &bytes)?)?;
    Ok(Fingerprint {
        sha256: hex(sha256_ctx.finish().as_ref()),
        size_bytes: bytes.len() as u64,
        top_dir,
    })
}

/// Lists a gzipped tarball's entries via the system `tar` (buckify is a
/// developer tool, not a build action, so ambient tools are fine).
///
/// `bytes` go through a temp file rather than a stdin pipe: piping both
/// directions at once deadlocks once the entry listing outgrows the
/// stdout pipe buffer (tar blocks writing it while we block writing
/// stdin — observed hung on a real registry tarball).
fn list_tarball(url: &str, bytes: &[u8]) -> anyhow::Result<Vec<String>> {
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "buckify-{}-{}.tgz",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, bytes).with_context(|| format!("staging {url} for tar"))?;
    let output = std::process::Command::new("tar")
        .args(["-tzf"])
        .arg(&path)
        .stderr(std::process::Stdio::null())
        .output()
        .with_context(|| format!("listing {url}"))?;
    std::fs::remove_file(&path).ok();
    if !output.status.success() {
        anyhow::bail!("`tar -tzf` failed for {url}");
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.to_string())
        .collect())
}

/// The single top-level directory wrapping every entry, or an error when
/// the tarball has none (files at the root) or several. Callers emit it
/// as `strip_prefix` (unless it's the `package` convention, which the
/// macro already defaults to).
fn wrapper_dir(url: &str, entries: &[String]) -> anyhow::Result<String> {
    let mut tops = std::collections::BTreeSet::new();
    for entry in entries {
        if entry.contains("@PaxHeader") {
            continue;
        }
        let mut cleaned = entry.as_str();
        while let Some(rest) = cleaned.strip_prefix("./") {
            cleaned = rest;
        }
        match cleaned.split_once('/') {
            Some((top, _)) if !top.is_empty() => {
                tops.insert(top.to_string());
            }
            _ => anyhow::bail!("{url}: no single top-level dir (entry {entry:?})"),
        }
    }
    if tops.len() != 1 {
        anyhow::bail!("{url}: no single top-level dir ({tops:?})");
    }
    Ok(tops.into_iter().next().expect("exactly one"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn detects_conventional_and_oddball_wrappers() {
        assert_eq!(
            wrapper_dir("u", &entries(&["package/", "package/index.js", "package/package.json"])).unwrap(),
            "package"
        );
        assert_eq!(
            wrapper_dir("u", &entries(&["estree/", "estree/index.d.ts"])).unwrap(),
            "estree"
        );
        // Leading ./ segments and pax headers don't confuse it.
        assert_eq!(
            wrapper_dir(
                "u",
                &entries(&["./package/", "./package/x.js", "package/@PaxHeader/x"])
            )
            .unwrap(),
            "package"
        );
    }

    #[test]
    fn flat_or_multi_root_tarballs_fail_loudly() {
        assert!(wrapper_dir("u", &entries(&["index.js", "package.json"])).is_err());
        assert!(wrapper_dir("u", &entries(&["a/x", "b/y"])).is_err());
        assert!(wrapper_dir("u", &[]).is_err());
    }

    // Piping the tarball through stdin while capturing the listing
    // deadlocks once *both* directions exceed the 64KiB pipe buffer
    // (tar blocks writing the listing while we block writing stdin).
    // This tarball is ~1.5MiB compressed with a ~150KiB listing, so it
    // hangs forever against that shape and passes through a temp file.
    #[test]
    fn big_tarball_listing_does_not_deadlock() {
        let dir = std::env::temp_dir().join(format!("buckify-test-{}", std::process::id()));
        let pkg = dir.join("package");
        std::fs::create_dir_all(&pkg).unwrap();
        // Xorshift: deterministic but incompressible (zeros would gzip
        // to nothing and never fill the stdin side of the old pipe).
        let mut state: u64 = 0x12345678;
        let mut chunk = vec![0u8; 300];
        for i in 0..5000 {
            for b in chunk.iter_mut() {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *b = (state >> 33) as u8;
            }
            std::fs::write(pkg.join(format!("f{i:05}.bin")), &chunk).unwrap();
        }
        let tgz = dir.join("big.tgz");
        let status = std::process::Command::new("tar")
            .args(["-czf"])
            .arg(&tgz)
            .arg("-C")
            .arg(&dir)
            .arg("package")
            .status()
            .unwrap();
        assert!(status.success());
        let bytes = std::fs::read(&tgz).unwrap();
        assert!(bytes.len() > 64 * 1024, "test tarball too small to deadlock");
        let listing = list_tarball("test-big.tgz", &bytes).unwrap();
        assert!(listing.len() > 5000, "unexpected listing: {}", listing.len());
        assert_eq!(wrapper_dir("test-big.tgz", &listing).unwrap(), "package");
        std::fs::remove_dir_all(&dir).ok();
    }
}
