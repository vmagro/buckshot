"""Test rule for wasm modules (see `wasm/wasm_module.bzl`).

`wasm_test` executes an exported function of a wasm module under wasmtime
(`wasmtime run --invoke`) and asserts on its stdout. The module is
typically a `wasm_module` wrapping a rust cdylib (see `tests/wasm/`),
but any target producing a runnable module works.

The macro pins `default_target_platform` to
`buckshot//platforms:wasm32`, so each test -- and its `module` dep, which
inherits the test's configuration -- targets wasm32 no matter where the
`wasm_test` is declared, with no `target_platform_detector_spec` entry
needed. Test execution itself stays on the build host: wasmtime and the
driver arrive via `exec_dep`.

Everything runs hermetically on the build host with no shell: the test
command is the `wasm_test_driver` bootstrap binary plus artifact argv, so
it behaves identically on mac/linux/windows hosts.

Lives in `wasm/` (not `rust/`): the rule, driver, and wasmtime releases
are wasm-specific artifacts shared by any wasm test, whatever language
produced the module.
"""

load("@prelude//test:inject_test_run_info.bzl", "inject_test_run_info")

def _wasm_test_impl(ctx):
    cmd = cmd_args(
        ctx.attrs.driver[RunInfo],
        ctx.attrs.wasmtime[DefaultInfo].default_outputs[0],
        ctx.attrs.module[DefaultInfo].default_outputs[0],
        ctx.attrs.invoke,
        ctx.attrs.args,
        "--",
        ctx.attrs.expected_stdout,
    )
    test_info = ExternalRunnerTestInfo(
        command = [cmd],
        type = "custom",
    )
    return inject_test_run_info(ctx, test_info) + [DefaultInfo()]

_wasm_test = rule(
    attrs = {
        "args": attrs.list(attrs.string(), default = [], doc = "Arguments to the invoked function."),
        "driver": attrs.exec_dep(
            default = "buckshot//wasm:wasm_test_driver",
            providers = [RunInfo],
        ),
        "expected_stdout": attrs.string(doc = "Expected stdout, compared after stripping surrounding whitespace."),
        "invoke": attrs.string(doc = "Exported function to invoke."),
        "module": attrs.dep(
            doc = "The wasm module to execute (e.g. a rust_library's `cdylib` subtarget: `:guest[cdylib]`).",
            providers = [DefaultInfo],
        ),
        "wasmtime": attrs.exec_dep(
            doc = "The wasmtime binary, as an `http_archive` file subtarget selected per host (e.g. `:wasmtime-aarch64-apple-darwin[wasmtime]`).",
            providers = [DefaultInfo],
        ),
        "_inject_test_env": attrs.default_only(attrs.dep(default = "prelude//test/tools:inject_test_env")),
    },
    impl = _wasm_test_impl,
)

def wasm_test(*, name, default_target_platform = "buckshot//platforms:wasm32", **kwargs):
    """Declare a test that runs a wasm module's exported function under wasmtime.

    Args:
        name: Test target name.
        default_target_platform: Target platform for the test (and its
            `module` dep). Overridable for future wasm variants; defaults
            to wasm32.
        **kwargs: `module`, `wasmtime`, `invoke`, `args`,
            `expected_stdout` (see the rule attrs), plus standard target
            kwargs like `visibility`.
    """
    _wasm_test(name = name, default_target_platform = default_target_platform, **kwargs)
