"""Test-only `tsc` probe: runs the toolchain's native `tsc --noEmit`.

`tsc_probe` typechecks `srcs` with `NodeToolchainInfo.tsc` and passes
iff tsc's exit code matches `expect` (`"pass"` = 0, `"fail"` =
nonzero). Exercises the real thing end to end: the exe runs, finds
its bundled `lib/*.d.ts` next to itself, and reports diagnostics.
"""

load("@buckshot//node/toolchain:node_toolchain.bzl", "NodeToolchainInfo")
load("@prelude//test:inject_test_run_info.bzl", "inject_test_run_info")

# $1 = expected outcome ("pass"/"fail"), $2 = tsc exe, rest = sources.
_TSC_CHECK_SH = """\
#!/usr/bin/env bash
want="$1"; exe="$2"; shift 2
"$exe" --noEmit "$@"
code=$?
if [ "$want" = "pass" ]; then test "$code" -eq 0; else test "$code" -ne 0; fi
"""


def _tsc_probe_impl(ctx):
    tsc = ctx.attrs._node_toolchain[NodeToolchainInfo].tsc
    if tsc == None:
        fail("tsc_probe: node toolchain has no tsc (regenerate it with --typescript-version)")
    script = ctx.actions.write("tsc_check.sh", _TSC_CHECK_SH, is_executable = True)
    cmd = cmd_args(script, ctx.attrs.expect, tsc)
    for src in ctx.attrs.srcs:
        cmd.add(src)
    test_info = ExternalRunnerTestInfo(
        command = [cmd],
        type = "tsc",
    )
    return inject_test_run_info(ctx, test_info) + [DefaultInfo()]


tsc_probe = rule(
    impl = _tsc_probe_impl,
    attrs = {
        "srcs": attrs.list(attrs.source()),
        "expect": attrs.enum(["pass", "fail"]),
        "_node_toolchain": attrs.toolchain_dep(
            default = "toolchains//:node",
            providers = [NodeToolchainInfo],
        ),
        "_inject_test_env": attrs.default_only(attrs.dep(default = "prelude//test/tools:inject_test_env")),
    },
)
