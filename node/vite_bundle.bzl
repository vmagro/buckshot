"""`vite_bundle` -- bundles a small web app with Vite, itself just another
package resolved via the exact same `JsPackageInfo`/`node_modules`
machinery as everything else in `node/` -- not a hidden `npm install` at
build time reaching out to the registry, and not whatever hoisting npm
happens to have done locally. `deps` must list every package the app
needs, explicitly: `vite` and `@vitejs/plugin-react` (both required by
`vite.config.js`, see below), the app's own runtime deps (`react`,
`react-dom`, a component library, ...), and any other Vite plugins.

`vite.config.js` is *not* part of `srcs` -- every `vite_bundle` gets the
exact same one (`node/vite.config.js`, exported via `node/BUCK` and
pulled in through the `vite_config` `attrs.default_only` dep, so a
target can't override it) automatically. It just wires up
`@vitejs/plugin-react` and `resolve.preserveSymlinks` (see `live` below)
-- if a real app ever needs its own plugins/config beyond that, this'll
need an `extra_config`-style escape hatch, but nothing here needs one yet.

`buck2 build :name` produces the built `dist/` directory (`DefaultInfo`).
Two `RunInfo` sub_targets cover the rest of a normal Vite workflow:

- `:name[serve]` -- `vite preview`, serving the already-built `dist/`.
- `:name[live]` -- `vite` with no subcommand, i.e. the live-reloading dev
  server, running directly against `srcs` (no prior build needed).

All three run `vite` with the work tree (`srcs` + the merged
`node_modules/` built from `deps`) passed as its `root` -- no ambient
`node_modules` or `package.json`-driven resolution. `vite`'s own
`RunInfo` (an `exec_dep`, since it's a build-time tool that has to run on
the machine doing the building) is invoked directly -- unmodified, just
with a subcommand appended -- rather than reimplementing "how to run
vite"; the one small shim in this file (`_ABS_OUTDIR_SH`) only resolves
`--outDir` to an absolute path at run time, since Vite resolves a
relative `--outDir` against `root` rather than cwd and buck2 has no
portable "absolute path of this artifact" primitive to hand it directly.

`build`/`serve` use a work tree built with `ctx.actions.copied_dir`, not
`symlinked_dir`: a source file's artifact is typically just a symlink to
its real checked-in repo path, and Vite/Rollup (like some other
bundlers) realpath every file they touch during a real *build*, which
would walk straight back out to that real path -- escaping the
assembled work tree entirely and losing the `node_modules/` sibling
`vite.config.*`/`index.html`/etc. need to sit next to. Real copies have
no symlink for realpath to chase through, so `copied_dir` sidesteps the
problem at the cost of physically duplicating the merged `node_modules/`
per bundle -- fine for a small app.

`live` gets its *own* `symlinked_dir` work tree instead: its whole
purpose is editing the real, checked-in `srcs` and seeing the dev server
live-reload, which a `copied_dir` snapshot can't do (buck2 never
re-copies it just because you edited the original file). Vite's dev
server hits the *exact same* realpath-escape issue as the production
build, though -- confirmed empirically, `--configLoader native` plus
`node --preserve-symlinks` fixes only the earliest step (loading
`vite.config.*` itself, which realpaths before that config's own
settings can take effect), so an app using `live` also needs its own
`vite.config.*` to set `resolve: { preserveSymlinks: true }` for
everything past that (its own `srcs`' imports, `node_modules`
resolution) to stay inside the symlinked tree instead of escaping back
to the checked-in repo path. See `tests/node/astryx_site/vite.config.js`
for a working example.
"""

load(":node_modules_tree.bzl", "build_node_modules_layout")
load(":providers.bzl", "JsPackageInfo")

# Vite's own `--outDir` resolves relative to `root` (`work`, here), not
# relative to cwd -- and buck2 has no portable "give me this artifact's
# absolute path" primitive to hand it one directly (paths may differ
# between local and remote execution), so this is the standard fix:
# resolve it to absolute at actual run time, then exec the real command
# (`vite`'s own `RunInfo`, unmodified) with it appended after `--outDir`.
_ABS_OUTDIR_SH = """\
#!/usr/bin/env bash
set -euo pipefail
orig_pwd=$(pwd)
case "$1" in /*) out="$1" ;; *) out="$orig_pwd/$1" ;; esac
shift
exec "$@" --outDir "$out"
"""

def _vite_bundle_impl(ctx):
    for src in ctx.attrs.srcs:
        if src.short_path == "vite.config.js":
            fail("vite_bundle: don't pass your own vite.config.js in `srcs` -- " +
                 "every vite_bundle target gets node/vite.config.js automatically.")

    # Flatten `deps` into one `node_modules/` -- same pattern as
    # `node_module_providers`, just not wrapped in a `JsPackageInfo` of
    # its own since a bundle isn't itself an npm package other targets
    # depend on.
    node_modules = {}
    for dep in ctx.attrs.deps:
        node_modules.update(dep[JsPackageInfo].node_modules)
    tree = build_node_modules_layout(ctx, "modules", node_modules)

    # `srcs` at their own relative paths, the shared `vite.config.js`,
    # and `node_modules/` is `vite`'s `root`. `build`/`serve` use a
    # real-copy tree (see this file's own doc); `live` gets a separate
    # symlinked one so editing the actual checked-in `srcs` is what the
    # dev server sees.
    work_srcs = {src.short_path: src for src in ctx.attrs.srcs}
    work_srcs["vite.config.js"] = ctx.attrs.vite_config[DefaultInfo].default_outputs[0]
    work_srcs["node_modules"] = tree.project("node_modules")
    work = ctx.actions.copied_dir("work", work_srcs)
    live_work = ctx.actions.symlinked_dir("live_work", work_srcs)

    vite = ctx.attrs.vite[RunInfo]
    abs_outdir = ctx.actions.write("abs_outdir.sh", _ABS_OUTDIR_SH, is_executable = True)

    dist = ctx.actions.declare_output("dist", dir = True)
    ctx.actions.run(
        cmd_args(abs_outdir, dist.as_output(), vite, "build", work, "--emptyOutDir"),
        category = "vite_build",
        identifier = ctx.label.name,
    )

    sub_targets = {
        "serve": [
            DefaultInfo(default_output = dist),
            RunInfo(args = cmd_args(abs_outdir, dist, vite, "preview", work)),
        ],
        "live": [
            DefaultInfo(default_output = live_work),
            RunInfo(args = cmd_args(
                "env",
                "NODE_OPTIONS=--preserve-symlinks",
                vite,
                live_work,
                "--configLoader",
                "native",
            )),
        ],
    }

    return [DefaultInfo(default_output = dist, sub_targets = sub_targets)]

vite_bundle = rule(
    impl = _vite_bundle_impl,
    attrs = {
        "srcs": attrs.list(
            attrs.source(),
            doc = "This app's own files (`index.html`, `src/**`), staged " +
                  "at their `short_path` -- so e.g. `index.html` alongside " +
                  "this target's `BUCK` file lands at the work tree root. " +
                  "Do *not* include a `vite.config.js` -- every target " +
                  "gets the same one automatically, see `vite_config`.",
        ),
        "deps": attrs.list(
            attrs.dep(providers = [JsPackageInfo]),
            doc = "Every npm package this app needs, explicitly -- `vite` " +
                  "itself (its config is loaded from inside the work " +
                  "tree, so it needs to resolve `import \"vite\"` same as " +
                  "any other bare specifier), any Vite plugins, and the " +
                  "app's own runtime deps. Nothing is inferred from a " +
                  "`package.json`.",
        ),
        "vite": attrs.exec_dep(
            providers = [RunInfo],
            default = "third-party//npm:vite[vite]",
            doc = "The `vite` executable itself, run to do the actual " +
                  "building/serving. An `exec_dep` (not a plain `dep`, " +
                  "and not part of `deps`) because it's a build-time tool " +
                  "that must run on the machine doing the building, " +
                  "regardless of what platform the app itself targets.",
        ),
        "vite_config": attrs.default_only(
            attrs.dep(
                providers = [DefaultInfo],
                default = "@buckshot//node:vite.config.js",
            ),
            doc = "The shared `vite.config.js` every `vite_bundle` gets -- " +
                  "`default_only` rejects any value a target tries to pass, " +
                  "so this can't be overridden per-target.",
        ),
    },
)
