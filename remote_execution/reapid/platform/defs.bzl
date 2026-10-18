# Cache-only execution platform for `reapid`.
#
# A copy of the stock `prelude//platforms:defs.bzl` `execution_platform`
# rule with the remote cache enabled: actions still run locally on the
# host configuration (`remote_enabled = False`), and the REAPI daemon
# serves purely as an action cache + CAS. Copying (rather than an empty
# `ConfigurationInfo`) keeps the host cpu/os constraints, which this
# repo's toolchain selects resolve against.

load("@prelude//cfg/exec_platform:marker.bzl", "get_exec_platform_marker")

def _reapi_cache_platform_impl(ctx: AnalysisContext) -> list[Provider]:
    constraints = dict()
    constraints.update(ctx.attrs.cpu_configuration[ConfigurationInfo].constraints)
    constraints.update(ctx.attrs.os_configuration[ConfigurationInfo].constraints)
    cfg = ConfigurationInfo(constraints = constraints, values = {})

    name = ctx.label.raw_target()
    platform = ExecutionPlatformInfo(
        configuration = cfg,
        executor_config = CommandExecutorConfig(
            allow_cache_uploads = True,
            local_enabled = True,
            remote_cache_enabled = True,
            remote_enabled = False,
            use_windows_path_separators = ctx.attrs.use_windows_path_separators,
        ),
        label = name,
    )

    return [
        DefaultInfo(),
        platform,
        PlatformInfo(configuration = cfg, label = str(name)),
        ExecutionPlatformRegistrationInfo(
            exec_marker_constraint = get_exec_platform_marker(),
            platforms = [platform],
        ),
    ]

reapi_cache_platform = rule(
    attrs = {
        "cpu_configuration": attrs.dep(providers = [ConfigurationInfo]),
        "os_configuration": attrs.dep(providers = [ConfigurationInfo]),
        "use_windows_path_separators": attrs.bool(),
    },
    impl = _reapi_cache_platform_impl,
)
