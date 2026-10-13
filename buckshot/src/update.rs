//! Updates buckshot's own CLI dotslash manifest (`tools/buck/buckshot`)
//! from a buckshot `cli-v*` GitHub release, published by the `CLI release`
//! workflow. (The buck2-domain manifests are managed by `buck update`.)
//!
//! The manifest is fetched byte-for-byte from the release's own assets
//! and validated (shebang, JSON body, `name` field, per-platform URLs)
//! before it is written.
//!
//! Run from the repo root (paths below are root-relative), or pass
//! `--root` to stage an update into another directory.

use std::path::PathBuf;

use anyhow::Context;
use clap::Args;

use crate::dotslash::make_executable;
use crate::dotslash::validate_dotslash;

const RELEASE_BASE: &str = "https://github.com/vmagro/buckshot/releases/download";
const RELEASES_API: &str = "https://api.github.com/repos/vmagro/buckshot/releases?per_page=30";

/// Release asset name -> path in the repo for buckshot's own CLI.
const DOTSLASH_FILE: (&str, &str) = ("buckshot", "tools/buck/buckshot");

#[derive(Args)]
pub struct UpdateArgs {
    /// CLI release tag, e.g. cli-v0.1.0 (default: newest cli-v* release).
    tag: Option<String>,

    /// Resolve the newest cli-v* release (the default when no tag).
    #[arg(long, conflicts_with = "tag")]
    latest: bool,

    /// Repo root to update (paths above resolve under it). Defaults to
    /// the current directory; point elsewhere to preview an update.
    #[arg(long, default_value = ".")]
    root: PathBuf,
}

/// Newest cli-v* buckshot release tag.
async fn resolve_latest(client: &reqwest::Client) -> anyhow::Result<String> {
    let releases: serde_json::Value = client
        .get(RELEASES_API)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .context("fetching buckshot releases")?
        .error_for_status()
        .context("fetching buckshot releases")?
        .json()
        .await
        .context("parsing buckshot releases")?;
    let empty = vec![];
    let list = releases.as_array().unwrap_or(&empty);
    for release in list {
        let tag = release.get("tag_name").and_then(serde_json::Value::as_str);
        if let Some(tag) = tag {
            if tag.starts_with("cli-v") {
                return Ok(tag.to_string());
            }
        }
    }
    anyhow::bail!("no cli-v* tag in the recent vmagro/buckshot releases")
}

pub async fn run(args: UpdateArgs) -> anyhow::Result<()> {
    // GitHub's API requires a User-Agent header on every request.
    let client = reqwest::Client::builder()
        .user_agent("buckshot")
        .build()
        .context("building HTTP client")?;

    let tag = match args.tag {
        Some(tag) => tag,
        None => resolve_latest(&client).await?,
    };
    // `--latest` is accepted for symmetry with `buck update`; it means
    // exactly what omitting the tag means.
    let _ = args.latest;
    eprintln!("updating to vmagro/buckshot release {tag}");

    let (name, relpath) = DOTSLASH_FILE;
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
    validate_dotslash(&data, name, &tag)?;
    let path = args.root.join(relpath);
    if path.exists() && std::fs::read(&path).ok().as_deref() == Some(&data[..]) {
        eprintln!("{relpath}: already current");
    } else {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, &data[..]).with_context(|| format!("writing {}", path.display()))?;
        make_executable(&path)?;
        eprintln!("{relpath}: updated to {tag} -- review with `sl status`");
    }

    Ok(())
}
