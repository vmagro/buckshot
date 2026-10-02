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

def _astral_python_impl(ctx):
    archive = ctx.attrs.archive[DefaultInfo].default_outputs[0]
    interpreter = archive.project("bin/python3")
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
        PythonPlatformInfo(name = ctx.attrs._platform_name),
    ]

astral_python = rule(
    attrs = {
        "archive": attrs.exec_dep(
            providers = [DefaultInfo],
        ),
        "_platform_name": attrs.default_only(
            attrs.string(
                default = select({
                    "prelude//cpu:arm64": "aarch64",
                    "prelude//cpu:x86_64": "x86_64",
                })
            )
        ),
    },
    impl = _astral_python_impl,
    is_toolchain_rule = True,
)
