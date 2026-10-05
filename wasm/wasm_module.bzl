"""Wrap a rust library's `cdylib` output as a standalone `.wasm` module.

`wasm_module` takes a rust library and exposes its `cdylib` build as this
target's output. That output already is a compiled `.wasm` file when the
dep is configured for `buckshot//platforms:wasm32` (the cxx toolchain's
`wasm` linker flavor names shared objects `*.wasm`), so the rule just
forwards the artifact -- and yes, "module" and `.wasm` are the right
names: a `.wasm` binary is a module, and it is what wasmtime runs.

The macro pins `default_target_platform` to wasm32 and
`target_compatible_with` to the wasm32 cpu constraint, so the module --
and its dep, which inherits the module's configuration -- always builds
for wasm no matter where the `wasm_module` is declared, with no
`target_platform_detector_spec` entry needed. `wasm_test` consumes these
modules (see `wasm/wasm_test.bzl`).
"""

def _wasm_module_impl(ctx):
    cdylib = ctx.attrs.dep[DefaultInfo].sub_targets.get("cdylib")
    if cdylib == None:
        fail("wasm_module `{}`: dep has no `cdylib` output (only rust libraries are supported)".format(ctx.label))
    return [DefaultInfo(default_output = cdylib[DefaultInfo].default_outputs[0])]

_wasm_module = rule(
    attrs = {
        "dep": attrs.dep(
            doc = "A rust library whose `cdylib` output becomes this module.",
            providers = [DefaultInfo],
        ),
    },
    impl = _wasm_module_impl,
)

def wasm_module(*, name, dep, default_target_platform = "buckshot//platforms:wasm32", target_compatible_with = None, **kwargs):
    """Declare a `.wasm` module built from a rust library's `cdylib`.

    Args:
        name: Target name; its output is the compiled `.wasm` file.
        dep: A rust library target (e.g. `":guest"`).
        default_target_platform: Target platform for the module (and its
            dep). Overridable for future wasm variants; defaults to wasm32.
        target_compatible_with: Compatibility constraints. Defaults to
            wasm32-cpu-only, matching the default platform.
    """
    _wasm_module(
        name = name,
        default_target_platform = default_target_platform,
        dep = dep,
        target_compatible_with = target_compatible_with or ["prelude//cpu/constraints:cpu[wasm32]"],
        **kwargs,
    )
