//! Updates everything this repo vendors from a facebook/buck2 GitHub
//! release: the dotslash manifests for the buck2 binary itself,
//! `starlark_fmt`, and `rust-project`, kept in sync with one release tag.
//!
//! The manifests are fetched byte-for-byte from the release's own assets
//! and validated (shebang, JSON body, `name` field, per-platform URLs)
//! before they are written. The prelude is not vendored: `.buckconfig`
//! declares it as a `bundled` external cell, so it always comes with the
//! buck2 binary as the tested-together pair.
//!
//! Run from the repo root (paths below are root-relative), or pass
//! `--root` to stage an update into another directory.

use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use clap::Args;

const RELEASE_BASE: &str = "https://github.com/facebook/buck2/releases/download";
const RELEASES_API: &str = "https://api.github.com/repos/facebook/buck2/releases?per_page=30";

/// Release asset name -> path in the repo, in update order.
const DOTSLASH_FILES: &[(&str, &str)] = &[
    ("buck2", "buck2"),
    ("starlark_fmt", "tools/buck/starlark_fmt"),
    ("rust-project", "tools/buck/rust-project"),
];

#[derive(Args)]
pub struct UpdateArgs {
    /// Release tag, e.g. 2026-10-01 (default: newest YYYY-MM-DD release).
    tag: Option<String>,

    /// Resolve the newest YYYY-MM-DD release (the default when no tag).
    #[arg(long, conflicts_with = "tag")]
    latest: bool,

    /// Repo root to update (paths above resolve under it). Defaults to
    /// the current directory; point elsewhere to preview an update.
    #[arg(long, default_value = ".")]
    root: PathBuf,
}

fn is_date_tag(tag: &str) -> bool {
    // YYYY-MM-DD, without pulling in a regex crate for one check.
    let b = tag.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .filter(|(i, _)| *i != 4 && *i != 7)
            .all(|(_, c)| c.is_ascii_digit())
}

/// Newest YYYY-MM-DD buck2 release tag.
///
/// Skips the rolling `latest` release: it ships only `.zst` blobs (no
/// dotslash manifests), so it can never be a vendor source here.
async fn resolve_latest(client: &reqwest::Client) -> anyhow::Result<String> {
    let releases: serde_json::Value = client
        .get(RELEASES_API)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .context("fetching buck2 releases")?
        .error_for_status()
        .context("fetching buck2 releases")?
        .json()
        .await
        .context("parsing buck2 releases")?;
    let empty = vec![];
    let list = releases.as_array().unwrap_or(&empty);
    for release in list {
        let tag = release.get("tag_name").and_then(serde_json::Value::as_str);
        if let Some(tag) = tag {
            if is_date_tag(tag) {
                return Ok(tag.to_string());
            }
        }
    }
    anyhow::bail!("no YYYY-MM-DD tag in the recent facebook/buck2 releases")
}

fn validate_dotslash(data: &[u8], name: &str, tag: &str) -> anyhow::Result<()> {
    let text = std::str::from_utf8(data)
        .with_context(|| format!("release {tag} asset {name:?} is not UTF-8 text"))?;
    let (shebang, body) = text
        .split_once('\n')
        .ok_or_else(|| anyhow::anyhow!("release {tag} asset {name:?} has no dotslash shebang"))?;
    if shebang != "#!/usr/bin/env dotslash" {
        anyhow::bail!(
            "release {tag} asset {name:?} has no dotslash shebang (is it still published as a dotslash manifest?)"
        );
    }
    let manifest: serde_json::Value = serde_json::from_str(body)
        .with_context(|| format!("release {tag} asset {name:?} has an unparsable manifest"))?;
    if manifest.get("name").and_then(serde_json::Value::as_str) != Some(name) {
        anyhow::bail!(
            "release {tag} asset {name:?} manifests as {:?}",
            manifest.get("name")
        );
    }
    let platforms = manifest
        .get("platforms")
        .and_then(serde_json::Value::as_object);
    let platforms = platforms
        .ok_or_else(|| anyhow::anyhow!("release {tag} asset {name:?} has no 'platforms' object"))?;
    for (platform, entry) in platforms {
        let url = entry
            .get("providers")
            .and_then(|p| p.get(0))
            .and_then(|p| p.get("url"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if !url.contains(&format!("/{tag}/")) {
            anyhow::bail!(
                "release {tag} asset {name:?} platform {platform:?} points outside the release: {url}"
            );
        }
    }
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .with_context(|| format!("statting {}", path.display()))?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("chmodding {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

async fn update_dotslash(client: &reqwest::Client, root: &Path, tag: &str) -> anyhow::Result<bool> {
    let mut changed = false;
    for (name, relpath) in DOTSLASH_FILES {
        let url = format!("{RELEASE_BASE}/{tag}/{name}");
        let data = client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("fetching {url}"))?
            .error_for_status()
            .with_context(|| format!("fetching {url}"))?
            .bytes()
            .await
            .with_context(|| format!("fetching {url}"))?;
        validate_dotslash(&data, name, tag)?;
        let path = root.join(relpath);
        if path.exists() && std::fs::read(&path).ok().as_deref() == Some(&data[..]) {
            eprintln!("{relpath}: already current");
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, &data[..]).with_context(|| format!("writing {}", path.display()))?;
        make_executable(&path)?;
        eprintln!("{relpath}: updated to {tag}");
        changed = true;
    }
    Ok(changed)
}

pub async fn update(args: UpdateArgs) -> anyhow::Result<()> {
    // GitHub's API requires a User-Agent header on every request.
    let client = reqwest::Client::builder()
        .user_agent("buckshot")
        .build()
        .context("building HTTP client")?;

    let tag = match args.tag {
        Some(tag) => tag,
        None => resolve_latest(&client).await?,
    };
    // `--latest` is accepted for symmetry with the old script; it means
    // exactly what omitting the tag means.
    let _ = args.latest;
    eprintln!("updating to facebook/buck2 release {tag}");

    if update_dotslash(&client, &args.root, &tag).await? {
        eprintln!("done -- review with `sl status`");
    } else {
        eprintln!("done -- already current");
    }

    Ok(())
}
