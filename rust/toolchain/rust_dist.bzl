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
    When the resolved host_bundle's own triple is a different
    linux-gnu arch than the target triple (building x86_64 Linux
    binaries on an aarch64 Linux host, say), it also sets
    `RustToolchainInfo.linker_flags` to `--target=<target triple>`
    so the cxx toolchain's linker (clang, which the prelude's rust
    build always links through) cross-links instead of targeting
    its own host arch.

Use `buck2 run //buckshot -- rust toolchain` to (re)generate the BUCK
file that wires these rules to specific component archives.
"""

load("@prelude//rust:rust_toolchain.bzl", "PanicRuntime", "RustToolchainInfo")

# ==========================================================================
# host_bundle
# ==========================================================================

HostBundleInfo = provider(
    fields = [
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
            cargo = ctx.attrs.cargo[DefaultInfo].default_outputs[0] if ctx.attrs.cargo else None,
            clippy = ctx.attrs.clippy[DefaultInfo].default_outputs[0] if ctx.attrs.clippy else None,
            host_triple = ctx.attrs.host_triple,
            rust_std_host = ctx.attrs.rust_std_host[DefaultInfo].default_outputs[0],
            rustc = ctx.attrs.rustc[DefaultInfo].default_outputs[0],
            rustfmt = ctx.attrs.rustfmt[DefaultInfo].default_outputs[0] if ctx.attrs.rustfmt else None,
        ),
    ]

host_bundle = rule(
    attrs = {
        "cargo": attrs.option(attrs.dep(), default = None),
        "clippy": attrs.option(attrs.dep(), default = None),
        "host_triple": attrs.string(),
        "rust_std_host": attrs.dep(),
        "rustc": attrs.dep(),
        "rustfmt": attrs.option(attrs.dep(), default = None),
    },
    impl = _host_bundle_impl,
)

# ==========================================================================
# rust_lld
# ==========================================================================

def _rust_lld_impl(ctx):
    bundle = ctx.attrs.host[HostBundleInfo]
    triple = bundle.host_triple

    if "windows" in triple:
        # Unchanged legacy behavior on Windows: point at the in-archive
        # binary (which was already dangling there — no `.exe` handling).
        # The assembly below needs `bash`/`cp`, so keep Windows lazy.
        rust_lld_path = cmd_args(
            bundle.rustc,
            format = "{}/lib/rustlib/" + triple + "/bin/rust-lld",
        )
        return [
            DefaultInfo(default_output = bundle.rustc),
            RunInfo(args = [rust_lld_path]),
        ]

    # Since ~2026-06 `rust-lld` dynamically links libLLVM, but the rustc
    # archive ships it at `lib/` while the binary's rpath
    # (`@loader_path/../lib` / `$ORIGIN/../lib`) expects it at
    # `lib/rustlib/<triple>/lib/` — invoking the in-archive binary fails
    # to load. Assemble a small dir with the loader-expected layout.
    # These must be real copies: a symlinked binary would resolve the
    # loader path back into the archive.
    out = ctx.actions.declare_output("rust-lld", dir = True)
    cmd = cmd_args(
        "/bin/bash",
        "-c",
        # Older nightlies (statically linked rust-lld) ship no libLLVM;
        # the `for` loop below is then a no-op.
        (
            'set -e; rustc="$1"; out="$2";' +
            ' mkdir -p "$out/bin" "$out/lib";' +
            ' cp "$rustc/lib/rustlib/{triple}/bin/rust-lld" "$out/bin/rust-lld";' +
            ' chmod +x "$out/bin/rust-lld";' +
            ' for lib in "$rustc"/lib/libLLVM.*; do' +
            ' [ -e "$lib" ] || continue;' +
            ' cp "$lib" "$out/lib/";' +
            " done"
        ).format(triple = triple),
        "_",
        bundle.rustc,
        out.as_output(),
    )
    ctx.actions.run(cmd, category = "assemble_rust_lld")
    return [
        DefaultInfo(default_output = out),
        RunInfo(args = cmd_args(out.project("bin/rust-lld"), hidden = [out])),
    ]

rust_lld = rule(
    attrs = {
        "host": attrs.exec_dep(providers = [HostBundleInfo]),
    },
    impl = _rust_lld_impl,
)

# ==========================================================================
# downloaded_rust_toolchain
# ==========================================================================

def _build_sysroot(ctx):
    bundle = ctx.attrs.host[HostBundleInfo]
    sysroot = ctx.actions.declare_output("sysroot", dir = True)

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

    ctx.actions.run(cmd, category = "rust_sysroot")
    return sysroot

def _cross_linker_flags(ctx):
    """`--target=<triple>` when genuinely cross-compiling linux-to-linux
    (e.g. an aarch64 Linux host building x86_64-unknown-linux-gnu).

    The prelude's rust build *always* links through the cxx toolchain's
    linker (`compile_ctx.linker_with_pre_args`, ultimately `clang++` here
    -- see rust/build.bzl and rust/context.bzl's `_linker`), appended to
    `rustc_cmd` as the final `-Clinker=...` -- so a `-Clinker=...` in this
    rule's own `rustc_flags` would just get silently overridden. The
    supported hook for extra args to *that* linker invocation is
    `RustToolchainInfo.linker_flags`, which context.bzl's `_linker` bakes
    straight into the generated `linker_wrapper.sh` alongside the cxx
    toolchain's own flags. Clang cross-compiles given just `--target=`
    (no separate cross binary needed, unlike gcc), provided the target's
    libc/crt objects are installed where it can find them (e.g. Debian's
    `crossbuild-essential-<arch>` multiarch layout).

    `bundle.host_triple` is the *execution* platform's native triple
    (known only at rule-impl time, via the exec_dep-resolved host_bundle)
    -- a plain BUCK-file-level select() can't see this, only the target
    triple, so this check has to live here rather than in the generated
    BUCK file.
    """
    bundle = ctx.attrs.host[HostBundleInfo]
    host_triple = bundle.host_triple
    target_triple = ctx.attrs.rustc_target_triple
    if host_triple != target_triple and host_triple.endswith("-linux-gnu") and target_triple.endswith("-linux-gnu"):
        return ["--target={}".format(target_triple)]
    return []

def _downloaded_rust_toolchain_impl(ctx):
    sysroot = _build_sysroot(ctx)

    rustc = RunInfo(cmd_args(sysroot.project("bin/rustc"), hidden = [sysroot]))
    rustdoc = RunInfo(cmd_args(sysroot.project("bin/rustdoc"), hidden = [sysroot]))
    clippy_driver = RunInfo(cmd_args(sysroot.project("bin/clippy-driver"), hidden = [sysroot]))
    rustfmt = RunInfo(cmd_args(sysroot.project("bin/rustfmt"), hidden = [sysroot]))

    sub_targets = {
        "rustfmt": [DefaultInfo(), rustfmt],
    }

    return [
        DefaultInfo(default_output = sysroot, sub_targets = sub_targets),
        RustToolchainInfo(
            allow_lints = ctx.attrs.allow_lints,
            clippy_driver = clippy_driver,
            clippy_toml = ctx.attrs.clippy_toml[DefaultInfo].default_outputs[0] if ctx.attrs.clippy_toml else None,
            compiler = rustc,
            default_edition = ctx.attrs.default_edition,
            deny_lints = ctx.attrs.deny_lints,
            deny_on_check_lints = ctx.attrs.deny_on_check_lints,
            doctests = ctx.attrs.doctests,
            linker_flags = _cross_linker_flags(ctx),
            nightly_features = ctx.attrs.nightly_features,
            panic_runtime = PanicRuntime("unwind"),
            report_unused_deps = ctx.attrs.report_unused_deps,
            rustc_binary_flags = ctx.attrs.rustc_binary_flags,
            rustc_flags = ctx.attrs.rustc_flags,
            rustc_target_triple = ctx.attrs.rustc_target_triple,
            rustc_test_flags = ctx.attrs.rustc_test_flags,
            rustdoc = rustdoc,
            rustdoc_flags = ctx.attrs.rustdoc_flags,
            sysroot_path = sysroot,
            warn_lints = ctx.attrs.warn_lints,
        ),
    ]

downloaded_rust_toolchain = rule(
    attrs = {
        "allow_lints": attrs.list(attrs.string(), default = []),
        "clippy_toml": attrs.option(attrs.dep(providers = [DefaultInfo]), default = None),
        "default_edition": attrs.option(attrs.string(), default = None),
        "deny_lints": attrs.list(attrs.string(), default = []),
        "deny_on_check_lints": attrs.list(attrs.string(), default = []),
        "doctests": attrs.bool(default = False),
        "host": attrs.exec_dep(
            doc = "`host_bundle` target carrying the host-side archives. "
            + "Resolved as exec_dep so the bundle's host-keyed selects "
            + "fire against the build host (= execution platform).",
            providers = [HostBundleInfo],
        ),
        "nightly_features": attrs.bool(default = False),
        "report_unused_deps": attrs.bool(default = False),
        "rust_std_target": attrs.dep(
            doc = "http_archive of `rust-std-<ver>-<target>.tar.xz`. Pass a "
            + "`select(...)` keyed on the consumer's target os/cpu — "
            + "this is what enables cross compiles to wasm32, etc.",
        ),
        "rustc_binary_flags": attrs.list(attrs.arg(), default = []),
        "rustc_flags": attrs.list(attrs.arg(), default = []),
        "rustc_target_triple": attrs.string(),
        "rustc_test_flags": attrs.list(attrs.arg(), default = []),
        "rustdoc_flags": attrs.list(attrs.arg(), default = []),
        "warn_lints": attrs.list(attrs.string(), default = []),
    },
    impl = _downloaded_rust_toolchain_impl,
    is_toolchain_rule = True,
)
