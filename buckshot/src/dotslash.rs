//! Shared dotslash-manifest helpers for the `buck update` and `update`
//! commands: manifest validation plus the executable bit.

use std::path::Path;

use anyhow::Context;

pub(crate) fn validate_dotslash(data: &[u8], name: &str, tag: &str) -> anyhow::Result<()> {
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
pub(crate) fn make_executable(path: &Path) -> anyhow::Result<()> {
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
pub(crate) fn make_executable(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}
