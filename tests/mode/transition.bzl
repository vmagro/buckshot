"""Test-only transition into `buckshot//mode:mode[release]`.

`release_transitioned` rebuilds its inner dep with the release mode
value layered onto whatever configuration the outer build uses — the
same shape downstream repos use to pin wasm artifacts (see frc_rs's
`macros/wasm.bzl`), minus the cpu/os/controller resets. Lets the mode
test exercise the toolchain's release flags without needing a
checked-in release platform for every host.
"""

def _release_transition_impl(platform, refs):
    release = refs.release[ConstraintValueInfo]
    constraints = dict(platform.configuration.constraints)
    constraints[release.setting.label] = release
    return PlatformInfo(
        configuration = ConfigurationInfo(
            constraints = constraints,
            values = platform.configuration.values,
        ),
        label = "release_mode_transition",
    )

_release_transition = transition(
    impl = _release_transition_impl,
    refs = {
        "release": "buckshot//mode:mode[release]",
    },
)

def _release_transitioned_impl(ctx):
    inner_default = ctx.attrs.inner[DefaultInfo]
    return [
        DefaultInfo(
            default_output = inner_default.default_outputs[0],
        ),
        # Forwarded verbatim (providers are immutable values), so
        # `buck2 run` on the wrapper executes the release binary.
        ctx.attrs.inner[RunInfo],
    ]

release_transitioned = rule(
    attrs = {
        "inner": attrs.dep(
            doc = "Binary target to rebuild under the release transition.",
            providers = [RunInfo],
        ),
    },
    cfg = _release_transition,
    impl = _release_transitioned_impl,
)
