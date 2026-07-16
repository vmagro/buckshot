use std::process::Command;

use anyhow::Context;

/// Fetches `url` via `curl` (rather than a Rust HTTP+TLS stack) so this tool
/// pulls in no crates with build scripts that shell out to a C compiler --
/// `curl` already exists on every machine this runs on and its TLS trust
/// store is more battle-tested than anything we'd vendor.
pub fn fetch_bytes(url: &str) -> anyhow::Result<Vec<u8>> {
    let output = Command::new("curl")
        .arg("-fsSL")
        .args(["-A", "buckshot"])
        .arg(url)
        .output()
        .context("running curl (is it installed?)")?;
    if !output.status.success() {
        anyhow::bail!(
            "curl exited with {} fetching {url}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output.stdout)
}
