"""Combines multiple execution-platform-providing targets into the single
ExecutionPlatformRegistrationInfo that `.buckconfig`'s `[build]
execution_platforms` key can point at (that key takes exactly one target).

Order matters: buck2 tries platforms in list order and uses the first one
where every exec dep of the target being built (its toolchains) is
compatible with that platform's configuration -- see
remote_execution_platform.bzl and the target's own `exec_compatible_with`
(e.g. tests/rust/BUCK) for the other half of this.
"""

def _execution_platforms_impl(ctx: AnalysisContext) -> list[Provider]:
    platforms = []
    for dep in ctx.attrs.platforms:
        platforms.extend(dep[ExecutionPlatformRegistrationInfo].platforms)

    return [
        DefaultInfo(),
        ExecutionPlatformRegistrationInfo(platforms = platforms),
    ]

execution_platforms = rule(
    impl = _execution_platforms_impl,
    attrs = {
        "platforms": attrs.list(attrs.dep(providers = [ExecutionPlatformRegistrationInfo])),
    },
)
