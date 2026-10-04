use std::collections::BTreeMap;

use anyhow::Context;
use serde::Serialize;

use super::manifest::Component;
use super::manifest::Manifest;
use super::manifest::{self};

#[derive(Serialize)]
#[serde(rename = "http_archive")]
struct HttpArchive {
    name: String,
    urls: Vec<String>,
    sha256: String,
    size_bytes: u64,
    strip_prefix: String,
    #[serde(rename = "type")]
    kind: String,
    visibility: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename = "select")]
struct Select<T>(BTreeMap<String, T>);

#[derive(Serialize)]
#[serde(rename = "host_bundle")]
struct HostBundle {
    name: String,
    rustc: Select<String>,
    rust_std_host: Select<String>,
    clippy: Select<String>,
    rustfmt: Option<Select<String>>,
    cargo: Option<Select<String>>,
    host_triple: Select<String>,
    visibility: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename = "rust_lld")]
struct RustLld {
    name: String,
    host: String,
    visibility: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename = "downloaded_rust_toolchain")]
struct DownloadedRustToolchain {
    name: String,
    host: String,
    rust_std_target: Select<String>,
    rustc_target_triple: Select<String>,
    default_edition: String,
    nightly_features: bool,
    deny_on_check_lints: Vec<String>,
    visibility: Vec<String>,
}

fn http_archive_for(comp: &Component) -> String {
    serde_starlark::to_string(&HttpArchive {
        name: comp.target_name.clone(),
        urls: vec![comp.url.clone()],
        sha256: comp.sha256.clone(),
        size_bytes: comp.size_bytes.expect("fetch_sizes runs before rendering"),
        strip_prefix: comp.strip_prefix.clone(),
        kind: "tar.xz".to_string(),
        visibility: vec![],
    })
    .expect("HttpArchive always serializes")
}

/// HEADs every component URL for its `Content-Length`, concurrently --
/// the channel TOML carries sha256 but no sizes, and `http_archive`
/// needs `size_bytes` alongside `sha256` or buck2 re-downloads the
/// archive after every daemon restart instead of recognizing the file
/// already on disk (breaking offline builds). Fails loudly if any URL
/// lacks a `Content-Length`: a missing size would silently reintroduce
/// the re-download.
async fn fetch_sizes(client: &reqwest::Client, comps: &mut [&mut Component]) -> anyhow::Result<()> {
    let mut set = tokio::task::JoinSet::new();
    for (i, comp) in comps.iter().enumerate() {
        let client = client.clone();
        let url = comp.url.clone();
        set.spawn(async move {
            let resp = client
                .head(&url)
                .send()
                .await
                .with_context(|| format!("HEAD {url}"))?
                .error_for_status()
                .with_context(|| format!("HEAD {url}"))?;
            // Parse the header by hand: reqwest 0.12's
            // `Response::content_length()` reports the *body* length (0
            // for a HEAD response), not this header.
            let len: u64 = resp
                .headers()
                .get(reqwest::header::CONTENT_LENGTH)
                .ok_or_else(|| anyhow::anyhow!("HEAD {url} returned no Content-Length"))?
                .to_str()
                .with_context(|| format!("HEAD {url} Content-Length is not ASCII"))?
                .parse()
                .with_context(|| format!("HEAD {url} Content-Length is not a number"))?;
            Ok::<_, anyhow::Error>((i, len))
        });
    }
    while let Some(res) = set.join_next().await {
        let (i, len) = res.context("size fetch task failed")??;
        comps[i].size_bytes = Some(len);
    }
    Ok(())
}

/// Target label for the canonical `config_setting` in platforms/configs/BUCK
/// matching a (cpu, os) pair.
fn platform_target(cpu: &str, os_name: Option<&str>) -> String {
    format!(
        "buckshot//platforms/configs:{}",
        manifest::platform_label(cpu, os_name)
    )
}

/// select() keyed on host platform, one arm per host triple, each pointing
/// at the matching component's own target (`:<target_name>`) -- these
/// resolve against the consumer's *execution* platform via `attrs.exec_dep`.
fn exec_select(
    host_triples: &[String],
    components: &[Component],
) -> anyhow::Result<Select<String>> {
    let mut map = BTreeMap::new();
    for (triple, comp) in host_triples.iter().zip(components) {
        let (cpu, os_name) = manifest::platform_for(triple)?;
        map.insert(
            platform_target(cpu, os_name),
            format!(":{}", comp.target_name),
        );
    }
    Ok(Select(map))
}

fn host_triple_select(host_triples: &[String]) -> anyhow::Result<Select<String>> {
    let mut map = BTreeMap::new();
    for triple in host_triples {
        let (cpu, os_name) = manifest::platform_for(triple)?;
        map.insert(platform_target(cpu, os_name), triple.clone());
    }
    Ok(Select(map))
}

/// select() keyed on target platform: every extra cross-compile target plus
/// every host triple gets one arm (so a host-as-target build picks the
/// host's own std), first-seen-by-label wins so an explicit `--target`
/// takes priority over a host triple mapping to the same (cpu, os) label.
/// (The default windows target, `x86_64-pc-windows-gnu`, relies on this to
/// beat the `x86_64-pc-windows-msvc` host triple for `windows-x86_64`.)
fn target_select(
    host_triples: &[String],
    extra_targets: &[String],
    value_for: impl Fn(&str) -> String,
) -> anyhow::Result<Select<String>> {
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    for triple in extra_targets.iter().chain(host_triples.iter()) {
        let (cpu, os_name) = manifest::platform_for(triple)?;
        map.entry(platform_target(cpu, os_name))
            .or_insert_with(|| value_for(triple));
    }
    Ok(Select(map))
}

pub struct RenderInput<'a> {
    pub channel_url: &'a str,
    pub manifest: &'a Manifest,
    pub host_triples: &'a [String],
    pub extra_targets: &'a [String],
    pub default_edition: &'a str,
    pub include_cargo: bool,
    pub include_rustfmt: bool,
}

pub async fn render(input: RenderInput<'_>) -> anyhow::Result<String> {
    let RenderInput {
        channel_url,
        manifest,
        host_triples,
        extra_targets,
        default_edition,
        include_cargo,
        include_rustfmt,
    } = input;

    if host_triples.is_empty() {
        anyhow::bail!("at least one host triple is required");
    }

    // One channel label for every component, derived from the release
    // (rustc) version -- component versions differ on stable.
    let channel = manifest::release_channel_label(manifest)?;

    let mut rustc = Vec::new();
    let mut rust_std_host = Vec::new();
    let mut clippy = Vec::new();
    let mut rustfmt = Vec::new();
    let mut cargo = Vec::new();

    for triple in host_triples {
        rustc.push(manifest::select_component(
            manifest,
            "rustc",
            triple,
            &format!("rustc-{triple}"),
            &channel,
        )?);
        rust_std_host.push(manifest::select_component(
            manifest,
            "rust-std",
            triple,
            &format!("rust-std-{triple}"),
            &channel,
        )?);
        clippy.push(manifest::select_component(
            manifest,
            "clippy-preview",
            triple,
            &format!("clippy-{triple}"),
            &channel,
        )?);
        if include_rustfmt {
            rustfmt.push(manifest::select_component(
                manifest,
                "rustfmt-preview",
                triple,
                &format!("rustfmt-{triple}"),
                &channel,
            )?);
        }
        if include_cargo {
            cargo.push(manifest::select_component(
                manifest,
                "cargo",
                triple,
                &format!("cargo-{triple}"),
                &channel,
            )?);
        }
    }

    // Cross-compile target std archives (skipping triples already in
    // hosts -- those reuse rust-std-<host>).
    let mut extra_std = Vec::new();
    for triple in extra_targets {
        if host_triples.contains(triple) {
            continue;
        }
        extra_std.push(manifest::select_component(
            manifest,
            "rust-std",
            triple,
            &format!("rust-std-{triple}"),
            &channel,
        )?);
    }

    // One shared client so the size HEADs below reuse connections.
    let client = reqwest::Client::new();
    let mut all: Vec<&mut Component> = rustc
        .iter_mut()
        .chain(&mut rust_std_host)
        .chain(&mut clippy)
        .chain(&mut rustfmt)
        .chain(&mut cargo)
        .chain(&mut extra_std)
        .collect();
    fetch_sizes(&client, &mut all).await?;

    let mut out = String::new();
    out.push_str(&format!(
        "# @generated by buckshot's `rust toolchain` command -- do not edit by hand.\n# Channel: {channel_url}\n\n"
    ));
    out.push_str("load(\"@prelude//:rules.bzl\", \"http_archive\")\n");
    out.push_str(
        "load(\"@buckshot//rust/toolchain:rust_dist.bzl\", \"downloaded_rust_toolchain\", \"host_bundle\", \"rust_lld\")\n\n",
    );

    // The helper programs (`rustc_wrapper`, `assemble_sysroot`,
    // `extract_rust_lld`) live in the static rust/toolchain/BUCK and are
    // shared by every versioned instance via the rule defaults in
    // rust_dist.bzl -- this file holds only this release's archives.
    let mut parts: Vec<String> = Vec::new();

    for comp in rustc
        .iter()
        .chain(&rust_std_host)
        .chain(&clippy)
        .chain(&rustfmt)
        .chain(&cargo)
        .chain(&extra_std)
    {
        parts.push(http_archive_for(comp));
    }

    // `host_bundle` holds host-side archives + the host_triple string. It's
    // pulled in via `attrs.exec_dep` from `rust_lld` and the toolchain, so
    // its host-keyed selects fire against the *execution* platform -- the
    // build host -- regardless of any target-platform transitions the
    // consumer applies (e.g. wasm32).
    parts.push(
        serde_starlark::to_string(&HostBundle {
            name: "host".to_string(),
            rustc: exec_select(host_triples, &rustc)?,
            rust_std_host: exec_select(host_triples, &rust_std_host)?,
            clippy: exec_select(host_triples, &clippy)?,
            rustfmt: include_rustfmt
                .then(|| exec_select(host_triples, &rustfmt))
                .transpose()?,
            cargo: include_cargo
                .then(|| exec_select(host_triples, &cargo))
                .transpose()?,
            host_triple: host_triple_select(host_triples)?,
            visibility: vec![],
        })
        .expect("HostBundle always serializes"),
    );

    parts.push(
        "\
# `rust-lld` extracted from the rustc archive as a regular file
# target so it can be referenced via `$(exe ...)` from
# non-toolchain rules (e.g. a wasm rust_library that needs
# `-Clinker=<rust-lld>` to override the host cxx toolchain).\n"
            .to_string()
            + &serde_starlark::to_string(&RustLld {
                name: "rust-lld".to_string(),
                host: ":host".to_string(),
                visibility: vec!["PUBLIC".to_string()],
            })
            .expect("RustLld always serializes"),
    );

    let non_host_extra_targets: Vec<String> = extra_targets
        .iter()
        .filter(|t| !host_triples.contains(t))
        .cloned()
        .collect();
    let rust_std_target = target_select(host_triples, &non_host_extra_targets, |t| {
        format!(":rust-std-{t}")
    })?;
    let rustc_target_triple =
        target_select(host_triples, &non_host_extra_targets, |t| t.to_string())?;

    let nightly_features = manifest
        .pkg
        .get("rustc")
        .map(|b| b.version.contains("-nightly"))
        .unwrap_or(false);

    parts.push(
        "\
# Surface clippy/rustc warnings as failures from `buck test :foo-check`
# without blocking the regular rust_library build path (see
# `deny_on_check_lints` below).\n"
            .to_string()
            + &serde_starlark::to_string(&DownloadedRustToolchain {
                name: "toolchain".to_string(),
                host: ":host".to_string(),
                rust_std_target,
                rustc_target_triple,
                default_edition: default_edition.to_string(),
                nightly_features,
                deny_on_check_lints: vec!["warnings".to_string()],
                visibility: vec!["PUBLIC".to_string()],
            })
            .expect("DownloadedRustToolchain always serializes"),
    );

    out.push_str(&parts.join("\n"));
    if !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

// ==========================================================================
// Rolling aliases (`rust/toolchains/BUCK`)
// ==========================================================================

#[derive(Serialize)]
#[serde(rename = "toolchain_alias")]
struct ToolchainAlias {
    name: String,
    actual: String,
    visibility: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename = "alias")]
struct Alias {
    name: String,
    actual: String,
    visibility: Vec<String>,
}

fn toolchain_alias_for(name: &str, actual: &str) -> String {
    serde_starlark::to_string(&ToolchainAlias {
        name: name.to_string(),
        actual: actual.to_string(),
        visibility: vec!["PUBLIC".to_string()],
    })
    .expect("ToolchainAlias always serializes")
}

/// Renders the rolling-alias BUCK file for the given releases: `nightly`
/// -> latest dated nightly, `stable` -> latest stable, `rust-lld` tracking
/// the same release as `nightly` (falling back to `stable` when no nightly
/// exists). Channels with no releases are omitted. Deterministic: same
/// release sets, same bytes.
pub fn render_aliases(nightlies: &[String], stables: &[String]) -> String {
    let mut nightlies = nightlies.to_vec();
    nightlies.sort();
    let mut stables = stables.to_vec();
    stables.sort_by_key(|v| crate::releases::semver_key(v));

    let latest_nightly = nightlies.last();
    let latest_stable = stables.last();

    let mut sections = Vec::new();
    if !nightlies.is_empty() {
        sections.push(format!("nightly [{}]", nightlies.join(", ")));
    }
    if !stables.is_empty() {
        sections.push(format!("stable [{}]", stables.join(", ")));
    }

    let mut out = String::new();
    out.push_str("# @generated by buckshot's `rust toolchain` command -- do not edit by hand.\n");
    out.push_str(&format!("# Releases: {}\n\n", sections.join("; ")));
    out.push_str("load(\"@prelude//:rules.bzl\", \"toolchain_alias\")\n\n");

    let mut parts: Vec<String> = Vec::new();
    if let Some(date) = latest_nightly {
        parts.push(toolchain_alias_for(
            "nightly",
            &format!("buckshot//rust/toolchains/nightly/{date}:toolchain"),
        ));
        parts.push(
            "# Default `rust-lld` for non-toolchain consumers (e.g. a wasm rust_library\n# passing `-Clinker=<rust-lld>`), tracking the same release as `:nightly`.\n"
                .to_string()
                + &serde_starlark::to_string(&Alias {
                    name: "rust-lld".to_string(),
                    actual: format!("buckshot//rust/toolchains/nightly/{date}:rust-lld"),
                    visibility: vec!["PUBLIC".to_string()],
                })
                .expect("Alias always serializes"),
        );
    }
    if let Some(version) = latest_stable {
        if latest_nightly.is_none() {
            parts.push(
                "# Default `rust-lld` for non-toolchain consumers (e.g. a wasm rust_library\n# passing `-Clinker=<rust-lld>`), tracking `:stable` (no nightly exists).\n"
                    .to_string()
                    + &serde_starlark::to_string(&Alias {
                        name: "rust-lld".to_string(),
                        actual: format!("buckshot//rust/toolchains/stable/{version}:rust-lld"),
                        visibility: vec!["PUBLIC".to_string()],
                    })
                    .expect("Alias always serializes"),
            );
        }
        parts.push(toolchain_alias_for(
            "stable",
            &format!("buckshot//rust/toolchains/stable/{version}:toolchain"),
        ));
    }

    out.push_str(&parts.join("\n"));
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Re-renders `dir/BUCK` (the rolling `nightly`/`stable` aliases) from the
/// versioned releases on disk. Leaves the file untouched when no releases
/// exist (e.g. generating to a scratch `--output` outside the repo tree).
pub fn refresh_aliases(dir: &std::path::Path) -> anyhow::Result<()> {
    let nightlies = crate::releases::scan_releases(&dir.join("nightly"))?;
    let stables = crate::releases::scan_releases(&dir.join("stable"))?;
    for date in &nightlies {
        if !crate::releases::is_date(date) {
            anyhow::bail!("{}/nightly/{date} is not a YYYY-MM-DD release", dir.display());
        }
    }
    for version in &stables {
        if crate::releases::semver_key(version).is_none() {
            anyhow::bail!(
                "{}/stable/{version} is not an X.Y.Z release",
                dir.display()
            );
        }
    }
    if nightlies.is_empty() && stables.is_empty() {
        eprintln!(
            "rust toolchain: no releases under {}, leaving {}/BUCK unchanged",
            dir.display(),
            dir.display()
        );
        return Ok(());
    }
    let rendered = render_aliases(&nightlies, &stables);
    std::fs::write(dir.join("BUCK"), rendered)
        .with_context(|| format!("writing {}/BUCK", dir.display()))?;
    eprintln!(
        "rust toolchain: refreshed {}/BUCK (nightly {}, stable {})",
        dir.display(),
        nightlies.iter().max().map(String::as_str).unwrap_or("-"),
        stables
            .iter()
            .max_by_key(|v| crate::releases::semver_key(v))
            .map(String::as_str)
            .unwrap_or("-"),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nightly_only_omits_stable() {
        let out = render_aliases(&["2026-07-16".to_string()], &[]);
        assert!(out.contains("# Releases: nightly [2026-07-16]\n"));
        assert!(out.contains("name = \"nightly\""));
        assert!(out.contains("buckshot//rust/toolchains/nightly/2026-07-16:toolchain"));
        assert!(out.contains("buckshot//rust/toolchains/nightly/2026-07-16:rust-lld"));
        assert!(!out.contains("name = \"stable\""));
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn latest_nightly_and_stable_win() {
        let out = render_aliases(
            &["2026-10-03".to_string(), "2026-07-16".to_string()],
            &["1.98.0".to_string(), "1.99.0".to_string()],
        );
        assert!(out.contains("nightly/2026-10-03:toolchain"));
        assert!(out.contains("nightly/2026-10-03:rust-lld"));
        assert!(out.contains("stable/1.99.0:toolchain"));
        assert!(!out.contains("2026-07-16:toolchain"));
        assert!(!out.contains("1.98.0:toolchain"));
    }

    #[test]
    fn stable_only_rust_lld_tracks_stable() {
        let out = render_aliases(&[], &["1.99.0".to_string()]);
        assert!(!out.contains("name = \"nightly\""));
        assert!(out.contains("stable/1.99.0:rust-lld"));
        assert!(out.contains("stable/1.99.0:toolchain"));
    }
}
