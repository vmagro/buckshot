"""Custom rust toolchain that pulls rustc + std libraries from a rustup
release channel via `http_archive`, instead of relying on whatever rustc
happens to be on `$PATH`.

Three rules collaborate:

  - `host_bundle` carries the host-side components (rustc binary, host
    rust-std, clippy, rustfmt, cargo). Its attrs are `select(...)` keyed
    on `prelude//os:*` / `prelude//cpu:*`. The toolchain references this
    bundle as `attrs.exec_dep`, which causes the bundle's analysis to
    happen in the *execution* platform — so the inner selects fire
    against the build host, not the consumer's target. Pure-target
    selects (a wasm32 attrs.dep) would resolve against the wasm32
    transition and there'd be no host arm to match.

  - `rust_lld` extracts the bundled `rust-lld` from inside the rustc
    archive. Same exec_dep pattern as above so non-toolchain consumers
    (e.g. a wasm cdylib link step) get the right host's rust-lld.

  - `downloaded_rust_toolchain` is the actual `RustToolchainInfo`
    provider. It takes `host_bundle` as exec_dep, plus a target-side
    `rust_std_target` (regular attrs.dep) so the std for the
    consumer's *target* triple lands in the merged sysroot. Cross
    compiles work via the same `--target=<triple>` flag flow as before.

Use `toolchains/rust/gen_rust_toolchain.py` to (re)generate the BUCK
file that wires these rules to specific component archives.
"""

load("@prelude//rust:rust_toolchain.bzl", "PanicRuntime", "RustToolchainInfo")

# ==========================================================================
# host_bundle
# ==========================================================================

HostBundleInfo = provider(
    fields=[
        "rustc",  # Artifact (unpacked rustc archive root)
        "rust_std_host",  # Artifact (unpacked rust-std-<host> root)
        "clippy",  # Artifact | None
        "rustfmt",  # Artifact | None
        "cargo",  # Artifact | None
        "host_triple",  # str — used to locate `rust-lld` inside the rustc dir
    ]
)


def _host_bundle_impl(ctx):
    return [
        DefaultInfo(),
        HostBundleInfo(
            rustc=ctx.attrs.rustc[DefaultInfo].default_outputs[0],
            rust_std_host=ctx.attrs.rust_std_host[DefaultInfo].default_outputs[0],
            clippy=ctx.attrs.clippy[DefaultInfo].default_outputs[0]
            if ctx.attrs.clippy
            else None,
            rustfmt=ctx.attrs.rustfmt[DefaultInfo].default_outputs[0]
            if ctx.attrs.rustfmt
            else None,
            cargo=ctx.attrs.cargo[DefaultInfo].default_outputs[0]
            if ctx.attrs.cargo
            else None,
            host_triple=ctx.attrs.host_triple,
        ),
    ]


host_bundle = rule(
    impl=_host_bundle_impl,
    attrs={
        "rustc": attrs.dep(),
        "rust_std_host": attrs.dep(),
        "clippy": attrs.option(attrs.dep(), default=None),
        "rustfmt": attrs.option(attrs.dep(), default=None),
        "cargo": attrs.option(attrs.dep(), default=None),
        "host_triple": attrs.string(),
    },
)

# ==========================================================================
# rust_lld
# ==========================================================================


def _rust_lld_impl(ctx):
    bundle = ctx.attrs.host[HostBundleInfo]

    # On Linux, `rust-lld` is dynamically linked against `libLLVM.so.*`
    # next to it (`$ORIGIN/../lib/libLLVM.so.*`). Copying just the
    # binary out into a buck-out subdir would orphan the RPATH, so we
    # keep the whole rustc archive as the materialized input and point
    # consumers at the in-archive `bin/rust-lld` path.
    rust_lld_path = cmd_args(
        bundle.rustc,
        format="{}/lib/rustlib/" + bundle.host_triple + "/bin/rust-lld",
    )
    return [
        DefaultInfo(default_output=bundle.rustc),
        RunInfo(args=[rust_lld_path]),
    ]


rust_lld = rule(
    impl=_rust_lld_impl,
    attrs={
        "host": attrs.exec_dep(providers=[HostBundleInfo]),
    },
)

# ==========================================================================
# downloaded_rust_toolchain
# ==========================================================================


def _build_sysroot(ctx):
    bundle = ctx.attrs.host[HostBundleInfo]
    sysroot = ctx.actions.declare_output("sysroot", dir=True)

    # Always-included parts: rustc (with its host std embedded), the host
    # rust-std archive, and the target's rust-std archive (which may be
    # the same as the host's when target == host — duplicate `cp -R` is a
    # no-op).
    parts = [
        bundle.rustc,
        bundle.rust_std_host,
        ctx.attrs.rust_std_target[DefaultInfo].default_outputs[0],
    ]
    if bundle.clippy:
        parts.append(bundle.clippy)
    if bundle.rustfmt:
        parts.append(bundle.rustfmt)
    if bundle.cargo:
        parts.append(bundle.cargo)

    cmd = cmd_args(
        "/bin/bash",
        "-c",
        'set -e; out="$1"; shift; mkdir -p "$out"; for src; do cp -R "$src/." "$out/"; done',
        "_",
        sysroot.as_output(),
    )
    for part in parts:
        cmd.add(part)

    ctx.actions.run(cmd, category="rust_sysroot")
    return sysroot


def _downloaded_rust_toolchain_impl(ctx):
    sysroot = _build_sysroot(ctx)

    rustc = RunInfo(args=[cmd_args(sysroot, format="{}/bin/rustc")])
    rustdoc = RunInfo(args=[cmd_args(sysroot, format="{}/bin/rustdoc")])
    clippy = RunInfo(args=[cmd_args(sysroot, format="{}/bin/clippy-driver")])

    return [
        DefaultInfo(default_output=sysroot),
        RustToolchainInfo(
            allow_lints=ctx.attrs.allow_lints,
            clippy_driver=clippy,
            clippy_toml=ctx.attrs.clippy_toml[DefaultInfo].default_outputs[0]
            if ctx.attrs.clippy_toml
            else None,
            compiler=rustc,
            default_edition=ctx.attrs.default_edition,
            deny_lints=ctx.attrs.deny_lints,
            deny_on_check_lints=ctx.attrs.deny_on_check_lints,
            doctests=ctx.attrs.doctests,
            nightly_features=ctx.attrs.nightly_features,
            panic_runtime=PanicRuntime("unwind"),
            report_unused_deps=ctx.attrs.report_unused_deps,
            rustc_binary_flags=ctx.attrs.rustc_binary_flags,
            rustc_flags=ctx.attrs.rustc_flags,
            rustc_target_triple=ctx.attrs.rustc_target_triple,
            rustc_test_flags=ctx.attrs.rustc_test_flags,
            rustdoc=rustdoc,
            rustdoc_flags=ctx.attrs.rustdoc_flags,
            sysroot_path=sysroot,
            warn_lints=ctx.attrs.warn_lints,
        ),
    ]


downloaded_rust_toolchain = rule(
    impl=_downloaded_rust_toolchain_impl,
    attrs={
        "host": attrs.exec_dep(
            providers=[HostBundleInfo],
            doc="`host_bundle` target carrying the host-side archives. "
            + "Resolved as exec_dep so the bundle's host-keyed selects "
            + "fire against the build host (= execution platform).",
        ),
        "rust_std_target": attrs.dep(
            doc="http_archive of `rust-std-<ver>-<target>.tar.xz`. Pass a "
            + "`select(...)` keyed on the consumer's target os/cpu — "
            + "this is what enables cross compiles to wasm32, etc.",
        ),
        "rustc_target_triple": attrs.string(),
        "default_edition": attrs.option(attrs.string(), default=None),
        "nightly_features": attrs.bool(default=False),
        "allow_lints": attrs.list(attrs.string(), default=[]),
        "deny_lints": attrs.list(attrs.string(), default=[]),
        "deny_on_check_lints": attrs.list(attrs.string(), default=[]),
        "warn_lints": attrs.list(attrs.string(), default=[]),
        "doctests": attrs.bool(default=False),
        "report_unused_deps": attrs.bool(default=False),
        "rustc_flags": attrs.list(attrs.arg(), default=[]),
        "rustc_binary_flags": attrs.list(attrs.arg(), default=[]),
        "rustc_test_flags": attrs.list(attrs.arg(), default=[]),
        "rustdoc_flags": attrs.list(attrs.arg(), default=[]),
        "clippy_toml": attrs.option(attrs.dep(providers=[DefaultInfo]), default=None),
    },
    is_toolchain_rule=True,
)
