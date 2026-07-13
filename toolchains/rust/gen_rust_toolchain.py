#!/usr/bin/env python3
"""Generate a buck2 rust toolchain BUCK file from a rustup release-channel TOML.

Reads the channel TOML at the given URL (e.g.
`https://static.rust-lang.org/dist/2026-04-27/channel-rust-nightly.toml`),
picks rustc / rust-std / clippy / rustfmt / cargo packages for every
requested host triple, plus rust-std for any cross-compile target, and
writes a BUCK file whose `downloaded_rust_toolchain` call uses

  - `select()` keyed on host os/cpu constraints for host-side components
    (rustc, clippy, rustfmt, cargo, rust_std_host) — these resolve in
    the consumer's *execution* platform via `attrs.exec_dep`, so the
    same toolchain target works on any supported build host.

  - `select()` keyed on target os/cpu constraints for the target rust-std
    + the `--target=` triple — these resolve in the consumer's *target*
    platform, so a wasm32 consumer pulls in the wasm rust-std without
    affecting which rustc binary runs.

Usage:

    python3 toolchains/rust/gen_rust_toolchain.py \\
        --channel-toml https://static.rust-lang.org/dist/2026-04-27/channel-rust-nightly.toml \\
        --host aarch64-apple-darwin \\
        --host x86_64-unknown-linux-gnu \\
        --host aarch64-unknown-linux-gnu \\
        --target wasm32-unknown-unknown \\
        --output toolchains/rust/BUCK

Re-run any time `rust-toolchain.toml` (or the upstream channel) changes;
the generated file is deterministic for a given input.
"""

from __future__ import annotations

import argparse
import sys
import textwrap
try:
    import tomllib
except ImportError:
    import tomli as tomllib  # type: ignore[no-redef]
from dataclasses import dataclass
from pathlib import Path
from typing import Any
from urllib.request import urlopen


@dataclass
class Component:
    """One entry in the generated BUCK: an http_archive of a component tarball."""

    target_name: str
    pkg: str  # rustup package id ("rustc", "rust-std", "clippy-preview", ...)
    triple: str
    url: str
    sha256: str
    strip_prefix: str  # `<outer>/<inner>` to drop the wrapper dirs


def _fetch_toml(url: str) -> dict[str, Any]:
    with urlopen(url) as resp:  # noqa: S310 — trusted rustup URL
        body = resp.read()
    return tomllib.loads(body.decode("utf-8"))


def _component_inner_dir(pkg: str, triple: str) -> str:
    """Inner directory inside a rustup component tarball.

    Tarballs unpack to `<outer>/<inner>/...`. The inner dir name matches
    the manifest's package id (including any `-preview` suffix) for most
    components. `rust-std` is the exception: its inner dir is
    `rust-std-<triple>`.
    """
    if pkg == "rust-std":
        return f"rust-std-{triple}"
    return pkg


def _outer_dir(pkg: str, channel: str, triple: str) -> str:
    """Top-level directory inside the tarball.

    The outer dir uses the URL package name (without `-preview`) — e.g.
    `clippy-nightly-<triple>`, not `clippy-preview-nightly-<triple>`.
    """
    short = pkg.removesuffix("-preview")
    return f"{short}-{channel}-{triple}"


def _channel_label(version: str) -> str:
    """Best-effort channel string used in tarball filenames.

    Nightly tarballs are `<pkg>-nightly-<triple>.tar.xz`, stable are
    `<pkg>-<version>-<triple>.tar.xz`, beta are `<pkg>-beta-<triple>.tar.xz`.
    The TOML's `pkg.<X>.version` is e.g. `"0.98.0-nightly (...)"` for
    nightly; sniff the suffix.
    """
    suffix = version.split(" ", 1)[0]
    if "-nightly" in suffix:
        return "nightly"
    if "-beta" in suffix:
        return "beta"
    return suffix


def _select_component(
    manifest: dict[str, Any],
    pkg: str,
    triple: str,
    *,
    target_name: str,
) -> Component:
    pkg_block = manifest["pkg"].get(pkg)
    if pkg_block is None:
        raise SystemExit(f"channel manifest has no [pkg.{pkg}] block")

    target_block = pkg_block["target"].get(triple)
    if target_block is None:
        raise SystemExit(
            f"[pkg.{pkg}.target.{triple}] not present in manifest "
            f"(typo? unsupported target?)",
        )
    if not target_block.get("available", False):
        raise SystemExit(f"[pkg.{pkg}.target.{triple}] is not `available = true`")

    xz_url = target_block.get("xz_url")
    xz_hash = target_block.get("xz_hash")
    if not xz_url or not xz_hash:
        raise SystemExit(f"[pkg.{pkg}.target.{triple}] missing xz_url/xz_hash")

    channel = _channel_label(pkg_block.get("version", ""))
    outer = _outer_dir(pkg, channel, triple)
    inner = _component_inner_dir(pkg, triple)

    return Component(
        target_name=target_name,
        pkg=pkg,
        triple=triple,
        url=xz_url,
        sha256=xz_hash,
        strip_prefix=f"{outer}/{inner}",
    )


# rustup target triple -> (cpu, os) for the matching prelude constraints.
# `os` is None for OS-less targets (wasm). Add new mappings as targets
# are requested.
_TRIPLE_TO_PLATFORM: dict[str, tuple[str, str | None]] = {
    "aarch64-apple-darwin": ("arm64", "macos"),
    "aarch64-unknown-linux-gnu": ("arm64", "linux"),
    "x86_64-apple-darwin": ("x86_64", "macos"),
    "x86_64-unknown-linux-gnu": ("x86_64", "linux"),
    "x86_64-pc-windows-msvc": ("x86_64", "windows"),
    "wasm32-unknown-unknown": ("wasm32", None),
    "wasm32-wasip1": ("wasm32", "wasi"),
}

# Platform label cache — avoids duplicating the os/cpu → config_setting name mapping
def _platform_label(cpu: str, os_name: str | None) -> str:
    """Map (cpu, os) to a local config_setting label."""
    if os_name is None:
        return f":cpu-{cpu}"
    return f":{os_name}-{cpu}"

_LINUX_ARM64_MAPPING = ("arm64", "linux")  # For aarch64-unknown-linux-gnu target

# Platform label cache — avoids duplicating the os/cpu → config_setting name mapping
def _platform_label(cpu: str, os_name: str | None) -> str:
    """Map (cpu, os) to a local config_setting label."""
    if os_name is None:
        return f":cpu-{cpu}"
    return f":{os_name}-{cpu}"

_LINUX_ARM64_MAPPING = ("arm64", "linux")  # For aarch64-unknown-linux-gnu target

# Ensure linux-arm64 config_setting is available for aarch64-unknown-linux-gnu
_LINUX_ARM64_MAPPING = ("arm64", "linux")


def _platform_for(triple: str) -> tuple[str, str | None]:
    if triple not in _TRIPLE_TO_PLATFORM:
        raise SystemExit(
            f"unknown cpu/os mapping for triple {triple!r}; add it to "
            f"_TRIPLE_TO_PLATFORM in toolchains/rust/gen_rust_toolchain.py",
        )
    return _TRIPLE_TO_PLATFORM[triple]


def _platform_label(cpu: str, os_name: str | None) -> str:
    """Local config_setting label for a (cpu, os) pair."""
    if os_name is None:
        # wasm32-unknown-unknown — cpu alone disambiguates.
        return f":cpu-{cpu}"
    return f":{os_name}-{cpu}"


_BUCK_HEADER = """\
# @generated by toolchains/rust/gen_rust_toolchain.py — do not edit by hand.
# Channel: {channel_url}

load("@prelude//:rules.bzl", "config_setting", "http_archive")
load("@toolchains//rust:rust_dist.bzl", "downloaded_rust_toolchain", "host_bundle", "rust_lld")
"""


_HTTP_ARCHIVE = """\
http_archive(
    name = "{name}",
    urls = ["{url}"],
    sha256 = "{sha256}",
    strip_prefix = "{strip_prefix}",
    type = "tar.xz",
    visibility = [],
)
"""


_CONFIG_SETTING_OS_CPU = """\
config_setting(
    name = "{os_name}-{cpu}",
    constraint_values = [
        "prelude//os/constraints:{os_name}",
        "prelude//cpu/constraints:{cpu}",
    ],
    visibility = [],
)
"""


_CONFIG_SETTING_CPU_ONLY = """\
config_setting(
    name = "cpu-{cpu}",
    constraint_values = ["prelude//cpu/constraints:{cpu}"],
    visibility = [],
)
"""


def _emit_archive(comp: Component) -> str:
    return _HTTP_ARCHIVE.format(
        name=comp.target_name,
        url=comp.url,
        sha256=comp.sha256,
        strip_prefix=comp.strip_prefix,
    )


def _select_for(arms: list[tuple[str, str]]) -> str:
    """Render a `select({...})` literal from (label, value) pairs.

    `value` is interpreted as a starlark expression (already-quoted strings,
    bare target labels, etc.) — pass `':foo'` not `":foo"` for string
    target labels.
    """
    lines = ["select({"]
    for label, value in arms:
        lines.append(f'        "{label}": {value},')
    lines.append("    })")
    return "\n".join(lines)


def _exec_select_arms(host_triples: list[str], component: str) -> list[tuple[str, str]]:
    """Build select() arms keyed on host platforms for a host-side component.

    Each host triple gets one arm pointing at `:<component>-<triple>`.
    """
    arms: list[tuple[str, str]] = []
    for triple in host_triples:
        cpu, os_name = _platform_for(triple)
        arms.append((_platform_label(cpu, os_name), f'":{component}-{triple}"'))
    return arms


def _target_select_arms(
    host_triples: list[str],
    extra_targets: list[str],
    *,
    value_for: callable,
) -> list[tuple[str, str]]:
    """Build select() arms keyed on target platforms.

    Every host triple PLUS every extra cross-compile target gets one arm
    (so a host-as-target build picks the host's std). `value_for(triple)`
    returns the starlark expression to use for that triple.
    """
    arms: list[tuple[str, str]] = []
    seen_labels: set[str] = set()
    for triple in [*host_triples, *extra_targets]:
        cpu, os_name = _platform_for(triple)
        label = _platform_label(cpu, os_name)
        if label in seen_labels:
            continue
        seen_labels.add(label)
        arms.append((label, value_for(triple)))
    return arms


def render(
    *,
    channel_url: str,
    manifest: dict[str, Any],
    host_triples: list[str],
    extra_targets: list[str],
    default_edition: str,
    include_cargo: bool,
    include_rustfmt: bool,
) -> str:
    if not host_triples:
        raise SystemExit("at least one --host triple is required")

    # Materialize per-host components.
    rustc: list[Component] = []
    rust_std_host: list[Component] = []
    clippy: list[Component] = []
    rustfmt: list[Component] = []
    cargo: list[Component] = []

    for triple in host_triples:
        rustc.append(
            _select_component(manifest, "rustc", triple, target_name=f"rustc-{triple}")
        )
        rust_std_host.append(
            _select_component(
                manifest, "rust-std", triple, target_name=f"rust-std-{triple}"
            )
        )
        clippy.append(
            _select_component(
                manifest,
                "clippy-preview",
                triple,
                target_name=f"clippy-{triple}",
            )
        )
        if include_rustfmt:
            rustfmt.append(
                _select_component(
                    manifest,
                    "rustfmt-preview",
                    triple,
                    target_name=f"rustfmt-{triple}",
                )
            )
        if include_cargo:
            cargo.append(
                _select_component(
                    manifest, "cargo", triple, target_name=f"cargo-{triple}"
                )
            )

    # Cross-compile target std archives (skipping triples already in hosts —
    # those reuse rust-std-<host>).
    extra_std: list[Component] = []
    for triple in extra_targets:
        if triple in host_triples:
            continue
        extra_std.append(
            _select_component(
                manifest, "rust-std", triple, target_name=f"rust-std-{triple}"
            )
        )

    # Header + archives.
    sections: list[str] = [_BUCK_HEADER.format(channel_url=channel_url)]
    archive_blocks: list[str] = []
    for comp in [*rustc, *rust_std_host, *clippy, *rustfmt, *cargo, *extra_std]:
        archive_blocks.append(_emit_archive(comp))
    sections.append("\n".join(archive_blocks))

    # config_setting targets — one per unique (cpu, os) pair across hosts +
    # targets. Wasm-style triples (no os) get a cpu-only setting.
    seen_platforms: set[tuple[str, str | None]] = set()
    cs_blocks: list[str] = []
    for triple in [*host_triples, *extra_targets]:
        cpu, os_name = _platform_for(triple)
        if (cpu, os_name) in seen_platforms:
            continue
        seen_platforms.add((cpu, os_name))
        if os_name is None:
            cs_blocks.append(_CONFIG_SETTING_CPU_ONLY.format(cpu=cpu))
        else:
            cs_blocks.append(_CONFIG_SETTING_OS_CPU.format(os_name=os_name, cpu=cpu))
    sections.append("\n".join(cs_blocks))

    # host_bundle holds host-side archives + the host_triple string. It's
    # pulled in via `attrs.exec_dep` from `rust_lld` and the toolchain, so
    # its host-keyed selects fire against the *execution* platform — the
    # build host — regardless of any target-platform transitions the
    # consumer applies (e.g. wasm32).
    rustc_select = _select_for(_exec_select_arms(host_triples, "rustc"))
    rust_std_host_select = _select_for(_exec_select_arms(host_triples, "rust-std"))
    clippy_select = _select_for(_exec_select_arms(host_triples, "clippy"))
    rustfmt_select = (
        _select_for(_exec_select_arms(host_triples, "rustfmt")) if rustfmt else None
    )
    cargo_select = (
        _select_for(_exec_select_arms(host_triples, "cargo")) if cargo else None
    )
    host_triple_select = _select_for(
        [
            (
                _platform_label(*_platform_for(triple)),
                f'"{triple}"',
            )
            for triple in host_triples
        ]
    )
    sections.append(
        textwrap.dedent("""\
            host_bundle(
                name = "host",
                rustc = {rustc},
                rust_std_host = {rust_std_host},
                clippy = {clippy},
                rustfmt = {rustfmt},
                cargo = {cargo},
                host_triple = {host_triple},
                visibility = [],
            )
        """).format(
            rustc=rustc_select,
            rust_std_host=rust_std_host_select,
            clippy=clippy_select,
            rustfmt=rustfmt_select if rustfmt_select is not None else "None",
            cargo=cargo_select if cargo_select is not None else "None",
            host_triple=host_triple_select,
        )
    )

    sections.append(
        textwrap.dedent("""\
            # `rust-lld` extracted from the rustc archive as a regular file
            # target so it can be referenced via `$(exe ...)` from
            # non-toolchain rules (e.g. a wasm rust_library that needs
            # `-Clinker=<rust-lld>` to override the host cxx toolchain).
            rust_lld(
                name = "rust-lld",
                host = ":host",
                visibility = ["PUBLIC"],
            )
        """)
    )

    rust_std_target_select = _select_for(
        _target_select_arms(
            host_triples,
            [t for t in extra_targets if t not in host_triples],
            value_for=lambda t: f'":rust-std-{t}"',
        )
    )
    rustc_target_triple_select = _select_for(
        _target_select_arms(
            host_triples,
            [t for t in extra_targets if t not in host_triples],
            value_for=lambda t: f'"{t}"',
        )
    )

    nightly = (
        "True" if "-nightly" in manifest["pkg"]["rustc"].get("version", "") else "False"
    )

    toolchain_block = textwrap.dedent("""\
        downloaded_rust_toolchain(
            name = "rust",
            host = ":host",
            rust_std_target = {rust_std_target},
            rustc_target_triple = {rustc_target_triple},
            default_edition = "{default_edition}",
            nightly_features = {nightly},
            # Surface clippy/rustc warnings as failures from `buck test :foo-check`
            # without blocking the regular rust_library build path.
            deny_on_check_lints = ["warnings"],
            visibility = ["PUBLIC"],
        )
    """).format(
        rust_std_target=rust_std_target_select,
        rustc_target_triple=rustc_target_triple_select,
        default_edition=default_edition,
        nightly=nightly,
    )
    sections.append(toolchain_block)

    return "\n".join(sections)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--channel-toml",
        required=True,
        help="URL to a rustup channel TOML.",
    )
    parser.add_argument(
        "--host",
        action="append",
        default=[
            "aarch64-apple-darwin",
            "x86_64-unknown-linux-gnu",
            "x86_64-pc-windows-msvc",
        ],
        dest="hosts",
        help="Host triple to support (repeatable). Each gets its own "
        "rustc/rust-std/clippy/rustfmt/cargo archives.",
    )
    parser.add_argument(
        "--target",
        action="append",
        default=[
            "wasm32-unknown-unknown",
            "aarch64-unknown-linux-gnu",
            "x86_64-unknown-linux-gnu",
            "aarch64-apple-darwin",
            "x86_64-pc-windows-msvc",
        ],
        dest="targets",
        help="Cross-compile target triple (repeatable). Adds a rust-std "
        "archive for each. Hosts double as their own target — no need to "
        "list them here.",
    )
    parser.add_argument("--default-edition", default="2024")
    parser.add_argument("--no-cargo", action="store_true")
    parser.add_argument("--no-rustfmt", action="store_true")
    parser.add_argument(
        "--output",
        type=Path,
        default=Path("toolchains/rust/BUCK"),
        help="Path to the BUCK file to write.",
    )

    args = parser.parse_args(argv)
    manifest = _fetch_toml(args.channel_toml)

    rendered = render(
        channel_url=args.channel_toml,
        manifest=manifest,
        host_triples=args.hosts,
        extra_targets=args.targets,
        default_edition=args.default_edition,
        include_cargo=not args.no_cargo,
        include_rustfmt=not args.no_rustfmt,
    )

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(rendered)
    print(f"wrote {args.output}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
