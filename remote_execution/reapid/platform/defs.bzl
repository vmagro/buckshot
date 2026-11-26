# Execution platform for `reapid`.
#
# A copy of the stock `prelude//platforms:defs.bzl` `execution_platform`
# rule with the remote cache enabled: every action runs locally on the
# build host (`local_enabled = True`, `remote_enabled = False`), while
# the REAPI daemon serves purely as a shared action cache + CAS (see
# `remote_execution/reapid/README.md`) -- and, with the store carried
# across machines, offline builds. The daemon also implements remote
# execution (covered by integration tests), but this platform never
# dispatches remotely; that keeps execution hermetic and deterministic.
# Copying (rather than an empty `ConfigurationInfo`) keeps the host
# cpu/os constraints, which this repo's toolchain selects resolve
# against.

load("@prelude//cfg/exec_platform:marker.bzl", "get_exec_platform_marker")

def _host_execution_properties() -> dict[str, str]:
    """Platform properties for actions this daemon host can run.

    Keys and values match the daemon's auto-detected worker identity
    (`std::env::consts::{OS, ARCH}`; see `reapid`'s `execution`
    config): `os` is one of linux/macos/windows, `arch` one of
    x86_64/aarch64 and friends. The daemon rejects actions whose stated
    requirements conflict. Unknown hosts omit the dimension they cannot
    name rather than mislabeling it.
    """
    props = {}
    os = host_info().os
    if os.is_linux:
        props["os"] = "linux"
    elif os.is_macos:
        props["os"] = "macos"
    elif os.is_windows:
        props["os"] = "windows"
    elif os.is_freebsd:
        props["os"] = "freebsd"
    arch = host_info().arch
    if arch.is_x86_64:
        props["arch"] = "x86_64"
    elif arch.is_aarch64:
        props["arch"] = "aarch64"
    elif arch.is_arm:
        props["arch"] = "arm"
    elif arch.is_riscv64:
        props["arch"] = "riscv64"
    return props

def _reapi_platform_impl(ctx: AnalysisContext) -> list[Provider]:
    constraints = dict()
    constraints.update(ctx.attrs.cpu_configuration[ConfigurationInfo].constraints)
    constraints.update(ctx.attrs.os_configuration[ConfigurationInfo].constraints)
    cfg = ConfigurationInfo(constraints = constraints, values = {})

    name = ctx.label.raw_target()
    platform = ExecutionPlatformInfo(
        configuration = cfg,
        executor_config = CommandExecutorConfig(
            allow_cache_uploads = True,
            local_enabled = False,
            remote_cache_enabled = True,
            remote_enabled = True,
            remote_execution_properties = _host_execution_properties(),
            remote_execution_use_case = "reapid",
            use_windows_path_separators = ctx.attrs.use_windows_path_separators,
        ),
        label = name,
    )

    return [
        DefaultInfo(),
        platform,
        PlatformInfo(configuration = cfg, label = str(name)),
        # Top-level copy so this platform can also be named in
        # `exec_compatible_with`, which resolves entries by their
        # `ConfigurationInfo`.
        cfg,
        ExecutionPlatformRegistrationInfo(
            exec_marker_constraint = get_exec_platform_marker(),
            platforms = [platform],
        ),
    ]

reapi_platform = rule(
    attrs = {
        "cpu_configuration": attrs.dep(providers = [ConfigurationInfo]),
        "os_configuration": attrs.dep(providers = [ConfigurationInfo]),
        "use_windows_path_separators": attrs.bool(),
    },
    impl = _reapi_platform_impl,
)
