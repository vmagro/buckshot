use std::collections::BTreeMap;
use std::collections::BTreeSet;

use maplit::btreemap;
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
    strip_prefix: String,
    #[serde(rename = "type")]
    kind: String,
    visibility: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename = "config_setting")]
struct ConfigSetting {
    name: String,
    constraint_values: Vec<String>,
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
    // Set directly on the toolchain (rather than per rust_binary/rust_library
    // target) so it applies to every consumer automatically -- toolchain
    // rules are analyzed using the *depending target's own* configuration
    // for non-exec_dep attrs like this one, so a linux-targeted build's
    // exec platform search rejects any exec platform where this toolchain
    // (and hence every rust target using it) would end up unable to
    // produce/link a Linux binary. See platforms/exec/README.md.
    exec_compatible_with: Select<Vec<String>>,
    visibility: Vec<String>,
}

fn http_archive_for(comp: &Component) -> String {
    serde_starlark::to_string(&HttpArchive {
        name: comp.target_name.clone(),
        urls: vec![comp.url.clone()],
        sha256: comp.sha256.clone(),
        strip_prefix: comp.strip_prefix.clone(),
        kind: "tar.xz".to_string(),
        visibility: vec![],
    })
    .expect("HttpArchive always serializes")
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
        let label = format!(":{}", manifest::platform_label(cpu, os_name));
        map.insert(label, format!(":{}", comp.target_name));
    }
    Ok(Select(map))
}

fn host_triple_select(host_triples: &[String]) -> anyhow::Result<Select<String>> {
    let mut map = BTreeMap::new();
    for triple in host_triples {
        let (cpu, os_name) = manifest::platform_for(triple)?;
        let label = format!(":{}", manifest::platform_label(cpu, os_name));
        map.insert(label, triple.clone());
    }
    Ok(Select(map))
}

/// select() keyed on target platform: every host triple plus every extra
/// cross-compile target gets one arm (so a host-as-target build picks the
/// host's own std), first-seen-by-label wins so a host triple always takes
/// priority over an extra target that maps to the same (cpu, os) label.
fn target_select(
    host_triples: &[String],
    extra_targets: &[String],
    value_for: impl Fn(&str) -> String,
) -> anyhow::Result<Select<String>> {
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    for triple in host_triples.iter().chain(extra_targets.iter()) {
        let (cpu, os_name) = manifest::platform_for(triple)?;
        let label = format!(":{}", manifest::platform_label(cpu, os_name));
        map.entry(label).or_insert_with(|| value_for(triple));
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

pub fn render(input: RenderInput) -> anyhow::Result<String> {
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
        )?);
        rust_std_host.push(manifest::select_component(
            manifest,
            "rust-std",
            triple,
            &format!("rust-std-{triple}"),
        )?);
        clippy.push(manifest::select_component(
            manifest,
            "clippy-preview",
            triple,
            &format!("clippy-{triple}"),
        )?);
        if include_rustfmt {
            rustfmt.push(manifest::select_component(
                manifest,
                "rustfmt-preview",
                triple,
                &format!("rustfmt-{triple}"),
            )?);
        }
        if include_cargo {
            cargo.push(manifest::select_component(
                manifest,
                "cargo",
                triple,
                &format!("cargo-{triple}"),
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
        )?);
    }

    let mut out = String::new();
    out.push_str(&format!(
        "# @generated by buckshot's `rust toolchain` command -- do not edit by hand.\n# Channel: {channel_url}\n\n"
    ));
    out.push_str("load(\"@prelude//:rules.bzl\", \"config_setting\", \"http_archive\")\n");
    out.push_str("load(\"@toolchains//rust:rust_dist.bzl\", \"downloaded_rust_toolchain\", \"host_bundle\", \"rust_lld\")\n\n");

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

    // config_setting targets -- one per unique (cpu, os) pair across hosts
    // + targets. Wasm-style triples (no os) get a cpu-only setting.
    let mut seen_platforms: BTreeSet<(String, Option<String>)> = BTreeSet::new();
    for triple in host_triples.iter().chain(extra_targets.iter()) {
        let (cpu, os_name) = manifest::platform_for(triple)?;
        seen_platforms.insert((cpu.to_string(), os_name.map(str::to_string)));
    }
    for (cpu, os_name) in &seen_platforms {
        let os_ref = os_name.as_deref();
        let constraint_values = match os_ref {
            None => vec![format!("prelude//cpu/constraints:{cpu}")],
            Some(os_name) => vec![
                format!("prelude//os/constraints:{os_name}"),
                format!("prelude//cpu/constraints:{cpu}"),
            ],
        };
        parts.push(
            serde_starlark::to_string(&ConfigSetting {
                name: manifest::platform_label(cpu, os_ref),
                constraint_values,
                visibility: vec![],
            })
            .expect("ConfigSetting always serializes"),
        );
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
                name: "rust".to_string(),
                host: ":host".to_string(),
                rust_std_target,
                rustc_target_triple,
                default_edition: default_edition.to_string(),
                nightly_features,
                deny_on_check_lints: vec!["warnings".to_string()],
                exec_compatible_with: Select(btreemap! {
                    "DEFAULT".to_string() => vec![],
                    "prelude//os:linux".to_string() => vec!["prelude//os/constraints:linux".to_string()],
                    "prelude//os:macos".to_string() => vec!["prelude//os/constraints:macos".to_string()],
                    "prelude//os:windows".to_string() => vec!["prelude//os/constraints:windows".to_string()],
                }),
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
