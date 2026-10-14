"""Test-only transition into the windows-arm64 target configuration.

`windows_arm64_transitioned` rebuilds its inner dep with the Windows OS,
ARM64 CPU, and GNU ABI constraints layered onto whatever configuration the
outer build uses -- the same shape `tests/mode/transition.bzl` uses for the
release mode. Lets the windows test exercise the toolchains'
windows-arm64 selects without needing `--target-platforms` on the invoking
command (a CLI platform cannot override a transition).
"""

def _windows_arm64_transition_impl(platform, refs):
    constraints = dict(platform.configuration.constraints)
    for ref in (refs.os, refs.cpu, refs.abi):
        value = ref[ConstraintValueInfo]
        constraints[value.setting.label] = value
    return PlatformInfo(
        configuration = ConfigurationInfo(
            constraints = constraints,
            values = platform.configuration.values,
        ),
        label = "windows_arm64_transition",
    )

_windows_arm64_transition = transition(
    impl = _windows_arm64_transition_impl,
    refs = {
        "abi": "prelude//abi/constraints:gnu",
        "cpu": "prelude//cpu/constraints:arm64",
        "os": "prelude//os/constraints:windows",
    },
)

def _windows_arm64_transitioned_impl(ctx):
    inner_default = ctx.attrs.inner[DefaultInfo]
    return [
        DefaultInfo(
            default_output = inner_default.default_outputs[0],
        ),
    ]

windows_arm64_transitioned = rule(
    attrs = {
        "inner": attrs.dep(
            doc = "Binary target to rebuild under the windows-arm64 transition.",
            providers = [DefaultInfo],
        ),
    },
    cfg = _windows_arm64_transition,
    impl = _windows_arm64_transitioned_impl,
)
