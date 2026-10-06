"""Wrap a rust or C/C++ library's shared output as a standalone `.wasm` module.

`wasm_module` takes a `rust_library` (using its `cdylib` output) or a
`cxx_library` (using its `shared` output) and exposes that build as this
target's output. That output already is a compiled `.wasm` file when the
dep is configured for `buckshot//platforms:wasm32` (the cxx toolchain's
`wasm` linker flavor names shared objects `*.wasm`), so the rule just
forwards the artifact -- and yes, "module" and `.wasm` are the right
names: a `.wasm` binary is a module, and it is what wasmtime runs.

For C/C++, the `zig_tool_wrapper` rewrites the prelude's `-shared` to
`--no-entry --export-all` (see `cxx/toolchain/zig_tool_wrapper.py`):
`wasm32-unknown-unknown` has no dynamic loader, so the shared link
becomes a static module exporting every symbol -- the same runnable
shape rustc's `cdylib` links produce.

The rule carries a `cfg` transition forcing the wasm32 cpu (and
dropping any OS half, matching `buckshot//platforms:wasm32` which is
CPU-only on purpose), so the module -- and its dep, which inherits the
module's configuration -- always builds for wasm no matter where the
`wasm_module` is declared and no matter what `--target-platforms` the
invoker passes (a CLI platform overrides `default_target_platform`,
but it cannot override a transition). The macro additionally pins
`default_target_platform` to wasm32 and `target_compatible_with` to
the wasm32 cpu constraint, so no `target_platform_detector_spec` entry
is needed. `wasm_test` consumes these modules (see
`wasm/wasm_test.bzl`).
"""

def _wasm32_transition_impl(platform, refs):
    cpu = refs.wasm32_cpu[ConstraintValueInfo]
    constraints = dict(platform.configuration.constraints)
    constraints[cpu.setting.label] = cpu
    # `buckshot//platforms:wasm32` has no OS half (`unknown-unknown`), so
    # drop whatever OS the incoming configuration carries (e.g. from a
    # CLI `--target-platforms` override) rather than setting one. The
    # `no_os` ref is only used for its setting's label.
    constraints.pop(refs.no_os[ConstraintValueInfo].setting.label, None)
    return PlatformInfo(
        label = "wasm32_wasm_module_transition",
        configuration = ConfigurationInfo(
            constraints = constraints,
            values = platform.configuration.values,
        ),
    )

_wasm32_transition = transition(
    impl = _wasm32_transition_impl,
    refs = {
        "wasm32_cpu": "prelude//cpu/constraints:cpu[wasm32]",
        "no_os": "prelude//os/constraints:none",
    },
)

def _wasm_module_impl(ctx):
    sub_targets = ctx.attrs.dep[DefaultInfo].sub_targets
    lib = sub_targets.get("cdylib") or sub_targets.get("shared")
    if lib == None:
        fail("wasm_module `{}`: dep has neither a `cdylib` (rust_library) nor a `shared` (cxx_library) output".format(ctx.label))
    return [DefaultInfo(default_output = lib[DefaultInfo].default_outputs[0])]

_wasm_module = rule(
    cfg = _wasm32_transition,
    attrs = {
        "dep": attrs.dep(
            doc = "A rust_library or cxx_library whose shared output (`cdylib` or `shared`) becomes this module.",
            providers = [DefaultInfo],
        ),
    },
    impl = _wasm_module_impl,
)

def wasm_module(*, name, dep, default_target_platform = "buckshot//platforms:wasm32", target_compatible_with = None, **kwargs):
    """Declare a `.wasm` module built from a library's shared output.

    Args:
        name: Target name; its output is the compiled `.wasm` file.
        dep: A rust_library or cxx_library target (e.g. `":guest"`).
        default_target_platform: Base target platform for the module
            (and its dep). The rule's transition forces the wasm32 cpu
            on top regardless; overridable for future wasm variants
            carrying extra constraints/values. Defaults to wasm32.
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
