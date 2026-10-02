"""An execution platform dispatched to a NativeLink remote-execution worker
(a local container -- see platforms/exec/Dockerfile) instead of running
actions locally.

macOS can't produce Linux ELF binaries, so any linux-targeted build needs
its actions to actually run somewhere Linux. This is deliberately NOT the
prelude's plain `execution_platform()` rule (see `platforms/defs.bzl` in
the prelude) -- that rule hardcodes `remote_enabled = False` in its
`CommandExecutorConfig`, so it can only ever run actions locally. Mirrors
buck2's own examples/remote_execution/buildbarn/platforms/defs.bzl.
"""

def _remote_execution_platform_impl(ctx: AnalysisContext) -> list[Provider]:
    constraints = dict()
    constraints.update(ctx.attrs.cpu_configuration[ConfigurationInfo].constraints)
    constraints.update(ctx.attrs.os_configuration[ConfigurationInfo].constraints)
    cfg = ConfigurationInfo(constraints = constraints, values = {})

    platform = ExecutionPlatformInfo(
        configuration = cfg,
        executor_config = CommandExecutorConfig(
            local_enabled = ctx.attrs.local_enabled,
            remote_enabled = True,
            remote_execution_properties = ctx.attrs.remote_execution_properties,
            remote_execution_use_case = "buck2-default",
            remote_output_paths = "output_paths",
            use_limited_hybrid = ctx.attrs.use_limited_hybrid,
        ),
        label = ctx.label.raw_target(),
    )

    return [
        DefaultInfo(),
        platform,
        PlatformInfo(configuration = cfg, label = str(ctx.label.raw_target())),
        ExecutionPlatformRegistrationInfo(platforms = [platform]),
    ]

remote_execution_platform = rule(
    attrs = {
        "cpu_configuration": attrs.dep(providers = [ConfigurationInfo]),
        # Keep False while actually verifying remote execution works at all
        # -- with local_enabled + use_limited_hybrid both True, buck2 may
        # silently run "remote" actions locally instead, defeating the
        # point of testing this. Worth revisiting once it's proven to work.
        "local_enabled": attrs.bool(default = False),
        "os_configuration": attrs.dep(providers = [ConfigurationInfo]),
        # Must be satisfiable by the worker's own advertised
        # platform_properties in platforms/exec/config.json5 -- these are
        # "exact"-match dimensions there, so values have to match verbatim.
        # No sensible default -- every caller needs to pick values matching
        # its actual worker.
        "remote_execution_properties": attrs.dict(key = attrs.string(), value = attrs.string()),
        "use_limited_hybrid": attrs.bool(default = False),
    },
    impl = _remote_execution_platform_impl,
)
