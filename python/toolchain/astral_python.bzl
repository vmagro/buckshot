"""Hermetic python toolchain backed by `python-build-standalone` archives."""

load(
    "@prelude//python:toolchain.bzl",
    "PythonPlatformInfo",
    "PythonToolchainInfo",
)
load(
    "@prelude//python_bootstrap:python_bootstrap.bzl",
    "PythonBootstrapToolchainInfo",
)

# ==========================================================================
# python_host_bundle
# ==========================================================================

PythonHostBundleInfo = provider(
    fields = [
        "archive",  # Artifact (unpacked install_only root)
        "os",  # str ("linux", "macos", or "windows") -- the *host* OS
        "platform_name",  # str -- platform tag for PythonPlatformInfo
    ],
)

def _python_host_bundle_impl(ctx):
    return [
        DefaultInfo(),
        PythonHostBundleInfo(
            archive = ctx.attrs.archive[DefaultInfo].default_outputs[0],
            os = ctx.attrs.os,
            platform_name = ctx.attrs.platform_name,
        ),
    ]

python_host_bundle = rule(
    attrs = {
        "archive": attrs.dep(doc = "`http_archive` of the `install_only` CPython distribution for this host. Pass a `select(...)` keyed on host os/cpu."),
        "os": attrs.string(doc = "Host OS the selected archive runs on. Pass a `select(...)` matching `archive`."),
        "platform_name": attrs.string(doc = "Platform tag for `PythonPlatformInfo`. Pass a `select(...)` matching `archive`."),
    },
    impl = _python_host_bundle_impl,
)

# ==========================================================================
# astral_python
# ==========================================================================

def _astral_python_impl(ctx):
    bundle = ctx.attrs.host[PythonHostBundleInfo]
    archive = bundle.archive
    # The windows `install_only` layout has no `bin/` dir: `python.exe`
    # sits at the archive root (after `strip_prefix`). `bundle.os` comes
    # from the host bundle's own host-keyed select, which fires in the
    # execution platform (same config as `archive` itself) -- never in the
    # consumer's target platform, which may not even have an OS (wasm32).
    interpreter = archive.project("python.exe" if bundle.os == "windows" else "bin/python3")
    return [
        DefaultInfo(
            sub_targets = {
                "bootstrap": [
                    DefaultInfo(),
                    PythonBootstrapToolchainInfo(interpreter = cmd_args(interpreter, hidden = [archive])),
                ],
                "interpreter": [
                    DefaultInfo(),
                    RunInfo(cmd_args(interpreter, hidden = [archive])),
                ],
            }
        ),
        PythonToolchainInfo(
            binary_linker_flags = [],
            compile = RunInfo(args = ["echo", "COMPILEINFO"]),
            host_interpreter = RunInfo(cmd_args(interpreter, hidden = [archive])),
            interpreter = RunInfo(cmd_args(interpreter, hidden = [archive])),
            linker_flags = [],
            native_link_strategy = "separate",
            package_style = "inplace",
            pex_extension = ".pex",
        ),
        PythonPlatformInfo(name = bundle.platform_name),
    ]

astral_python = rule(
    attrs = {
        "host": attrs.exec_dep(
            doc = "`python_host_bundle` carrying the host's interpreter archive. "
            + "Resolved as exec_dep so the bundle's host-keyed selects "
            + "fire against the build host (= execution platform), even "
            + "when the toolchain itself is analyzed in an exotic target "
            + "configuration (e.g. OS-less wasm32).",
            providers = [PythonHostBundleInfo],
        ),
    },
    impl = _astral_python_impl,
    is_toolchain_rule = True,
)
