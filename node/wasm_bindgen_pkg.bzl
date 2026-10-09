"""`wasm_bindgen_pkg` -- post-process a `.wasm` module (typically a
`wasm_module` output) with wasm-bindgen into an npm-shaped `wasm-pkg/`
directory: post-processed `.wasm`, JS shims, `.d.ts`, and a minimal
`package.json`, exposed as `JsPackageInfo` so any `node_modules` tree or
`vite_bundle` `deps` entry can consume it like any other package.

The actual bindgen work is done by `runner`, an injected executable
(this repo ships none -- bring your own): it is invoked once as

    runner --out=<dir> --input=<stem.wasm> --pkg-name=<name> --pkg-version=<ver>

and must write the package content (including `package.json`) under
`--out`. The input filename is normalized to `<pkg_name with `-` ->
`_`>.wasm` first, since wasm-bindgen names its outputs after the input
file's stem while the emitted `package.json` uses the package-name stem.
"""

load(":node_module.bzl", "node_module_providers")

def _wasm_bindgen_pkg_impl(ctx):
    out = ctx.actions.declare_output("wasm-pkg", dir = True)
    wasm = ctx.actions.copy_file(
        ctx.attrs.pkg_name.replace("-", "_") + ".wasm",
        ctx.attrs.wasm,
    )
    cmd = cmd_args(ctx.attrs.runner[RunInfo])
    cmd.add(cmd_args(out.as_output(), format = "--out={}"))
    cmd.add(cmd_args(wasm, format = "--input={}"))
    cmd.add("--pkg-name=" + ctx.attrs.pkg_name)
    cmd.add("--pkg-version=" + ctx.attrs.pkg_version)
    ctx.actions.run(cmd, category = "wasm_bindgen")
    return node_module_providers(
        ctx,
        bin = {},
        deps = [],
        # Generated action output -- never changes without a whole new
        # build. See `JsPackageInfo.immutable`'s own doc.
        immutable = True,
        package_dir = out,
        package_name = ctx.attrs.pkg_name,
    )

wasm_bindgen_pkg = rule(
    attrs = {
        "pkg_name": attrs.string(
            doc = 'npm package name for the emitted `package.json`, e.g. "my-wasm".',
        ),
        "pkg_version": attrs.string(
            default = "0.1.0",
            doc = "Version for the emitted `package.json`.",
        ),
        "runner": attrs.exec_dep(
            doc = "The wasm-bindgen executable -- an `exec_dep` since it runs "
            + "on the build host. Invoked as `runner --out=<dir> "
            + "--input=<stem.wasm> --pkg-name=<name> --pkg-version=<ver>`; "
            + "must write the package content under `--out`.",
            providers = [RunInfo],
        ),
        "wasm": attrs.source(
            doc = "The `.wasm` module to post-process (e.g. a `wasm_module` target).",
        ),
    },
    impl = _wasm_bindgen_pkg_impl,
)
