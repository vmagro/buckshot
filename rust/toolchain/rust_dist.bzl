"""Custom rust toolchain that pulls rustc + std libraries from a rustup
release channel via `http_archive`, instead of relying on whatever rustc
happens to be on `$PATH`.

Two rules collaborate:

  - `host_bundle` carries the host-side components (rustc binary, host
    rust-std, clippy, rustfmt, cargo). Its attrs are `select(...)` keyed
    on `prelude//os:*` / `prelude//cpu:*`. The toolchain references this
    bundle as `attrs.exec_dep`, which causes the bundle's analysis to
    happen in the *execution* platform — so the inner selects fire
    against the build host, not the consumer's target. A bare target-side
    select would resolve against the consumer's target config instead,
    which may not even have a host arm (e.g. OS-less wasm32).

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

`windows-gnu` and `dlltool`:

rustc compiles `#[link(kind = "raw-dylib")]` externs (used pervasively by
`windows-link` / `windows-sys`, hence by most crates that touch Win32 --
`getrandom`, `tempfile`, ...) by shelling out to binutils `dlltool` to
synthesize an import library. It looks up a target-prefixed binary
(`x86_64-w64-mingw32-dlltool`) on `PATH`, which a bare hermetic sysroot
does not provide. Following `cargo-zigbuild`, the toolchain therefore
prefixes `rustc`/`clippy-driver`/`rustdoc` with the `:rustc_wrapper`
`python_bootstrap_binary`, which provisions target-prefixed `dlltool`
shims (forwarding to `zig dlltool` from the cxx toolchain's Zig bundle)
on POSIX hosts; Windows hosts run the compiler unchanged (an
extensionless `sh` trampoline is not executable via `CreateProcess`, so
windows-*host* builds of `raw-dylib` crates remain unsupported -- the one
remaining windows-host gap). See `rustc_wrapper.py`.

All helper programs here (`rustc_wrapper.py`, `assemble_sysroot.py`)
run on the hermetic bootstrap interpreter: no sh/bash is involved
anywhere, on any host OS.
"""

load("@buckshot//cxx/toolchain:zig_toolchain.bzl", "ZigHostBundleInfo")
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
        "host_triple",  # str — the host triple, e.g. for `.exe` suffixing
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
# downloaded_rust_toolchain
# ==========================================================================

def _build_sysroot(ctx):
    bundle = ctx.attrs.host[HostBundleInfo]
    sysroot = ctx.actions.declare_output("sysroot", dir = True)

    # Always-included parts: rustc (with its host std embedded), the host
    # rust-std archive, and the target's rust-std archive (which may be
    # the same as the host's when target == host — copying it twice is a
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
        ctx.attrs.assemble[RunInfo],
        sysroot.as_output(),
    )
    for part in parts:
        cmd.add(part)

    ctx.actions.run(cmd, category = "rust_sysroot")
    return sysroot

def _cross_linker_flags(ctx):
    """Extra flags for the cxx toolchain linker invocation (see
    rust/context.bzl's `_linker`, which bakes `RustToolchainInfo.linker_flags`
    into the generated `linker_wrapper.sh` alongside the cxx toolchain's own
    flags).

    Currently always empty: the cxx toolchain is `zig c++`, which already
    receives its own `-target <zig triple>` for the consumer's target
    platform and cross-links with its bundled sysroots -- no
    `--target=<rust triple>` override needed. (Such a flag would be actively
    harmful: Zig rejects rust-style triples like `x86_64-unknown-linux-gnu`
    with "unable to parse target query ... UnknownOperatingSystem".)
    """
    return []

def _downloaded_rust_toolchain_impl(ctx):
    sysroot = _build_sysroot(ctx)

    # The wrapper provisions rustc's windows-gnu `dlltool` shims at
    # startup on POSIX hosts (see `rustc_wrapper.py`) and runs the
    # compiler unchanged everywhere else, so it prefixes the compiler
    # unconditionally on all host OSes.
    zig_bundle = ctx.attrs.zig_host[ZigHostBundleInfo]
    zig_exe = zig_bundle.archive.project("zig.exe" if zig_bundle.os == "windows" else "zig")
    wrapper = ctx.attrs.wrapper[RunInfo]
    hidden = [sysroot, zig_bundle.archive]

    def wrap(compiler):
        return RunInfo(cmd_args(wrapper, zig_exe, compiler, hidden = hidden))

    exe = ".exe" if "windows" in ctx.attrs.host[HostBundleInfo].host_triple else ""
    rustc = wrap(sysroot.project("bin/rustc" + exe))
    rustdoc = wrap(sysroot.project("bin/rustdoc" + exe))
    clippy_driver = wrap(sysroot.project("bin/clippy-driver" + exe))
    rustfmt = RunInfo(cmd_args(sysroot.project("bin/rustfmt" + exe), hidden = [sysroot]))

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
        "assemble": attrs.exec_dep(
            default = "buckshot//rust/toolchain:assemble_sysroot",
            doc = "`python_bootstrap_binary` running `assemble_sysroot.py` on the hermetic bootstrap interpreter. Merges the component archives into the sysroot dir.",
            providers = [RunInfo],
        ),
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
        "wrapper": attrs.exec_dep(
            default = "buckshot//rust/toolchain:rustc_wrapper",
            doc = "`python_bootstrap_binary` running `rustc_wrapper.py` on the hermetic bootstrap interpreter. Prefixes rustc/rustdoc/clippy-driver (same binary for all host OSes -- no sh/bash involved).",
            providers = [RunInfo],
        ),
        "zig_host": attrs.exec_dep(
            default = "buckshot//cxx/toolchains:host",
            doc = "The cxx toolchain's `zig_host_bundle` (host-side Zig "
            + "archive). Supplies `zig dlltool` for the compiler wrapper's "
            + "windows-gnu dlltool shims.",
            providers = [ZigHostBundleInfo],
        ),
    },
    impl = _downloaded_rust_toolchain_impl,
    is_toolchain_rule = True,
)
