"""`vite_bundle` -- bundles a small web app with Vite, itself just another
package resolved via the exact same `JsPackageInfo`/`node_modules`
machinery as everything else in `node/` -- not a hidden `npm install` at
build time reaching out to the registry, and not whatever hoisting npm
happens to have done locally. `deps` must list every package the app
needs, explicitly: any Vite plugins (e.g. `@vitejs/plugin-react`), the
app's own runtime deps (`react`, `react-dom`, a component library, ...),
and -- since `vite.config.*` itself does `import ... from "vite"` -- Vite
too.

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
"""

load(":node_modules_tree.bzl", "build_node_modules_layout")
load(":providers.bzl", "JsPackageInfo")

# `ctx.actions.symlinked_dir` doesn't work for this: a source file's
# artifact is typically just a symlink to its real checked-in repo path,
# and Vite/Rollup (like some other bundlers) realpath every file they
# touch, which walks straight back out to that real path -- escaping the
# assembled work tree entirely and losing the `node_modules/` sibling
# `vite.config.*`, `index.html`, etc. need to sit next to. Hardlinks have
# no separate "target" for realpath to chase (same reasoning as
# `node_modules_tree.bzl`'s own `NODE_MODULES_TREE_PY`), so this mirrors
# `srcs` the same way, plus one entry for the whole merged
# `node_modules/` dir (already real hardlinked directories internally, so
# mirroring it a second time here would be redundant -- recurse into it
# directly instead of hardlinking its container).
_WORK_TREE_PY = """\
import os
import sys


def mirror(src, dst):
    os.makedirs(dst, exist_ok=True)
    for entry in os.scandir(src):
        sp, dp = entry.path, os.path.join(dst, entry.name)
        if entry.is_dir(follow_symlinks=True):
            mirror(sp, dp)
        else:
            os.link(sp, dp)


def main():
    out = sys.argv[1]
    os.makedirs(out, exist_ok=True)
    rest = sys.argv[2:]
    for i in range(0, len(rest), 2):
        relpath, real = rest[i], rest[i + 1]
        dest = os.path.join(out, relpath)
        os.makedirs(os.path.dirname(dest), exist_ok=True)
        if os.path.isdir(real):
            mirror(real, dest)
        else:
            os.link(real, dest)


if __name__ == "__main__":
    main()
"""

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
    # Flatten `deps` into one `node_modules/` -- same pattern as
    # `node_module_providers`, just not wrapped in a `JsPackageInfo` of
    # its own since a bundle isn't itself an npm package other targets
    # depend on.
    node_modules = {}
    for dep in ctx.attrs.deps:
        node_modules.update(dep[JsPackageInfo].node_modules)
    tree = build_node_modules_layout(ctx, "modules", node_modules)

    # One work tree -- `srcs` at their own relative paths plus
    # `node_modules/` -- is `vite`'s `root` for the build, `serve`, and
    # `live` sub_targets alike.
    script = ctx.actions.write("work_tree.py", _WORK_TREE_PY)
    work = ctx.actions.declare_output("work", dir = True)
    cmd = cmd_args("python3", script, work.as_output())
    for src in ctx.attrs.srcs:
        cmd.add(src.short_path)
        cmd.add(src)
    cmd.add("node_modules")
    cmd.add(tree.project("node_modules"))
    ctx.actions.run(cmd, category = "vite_work_tree", identifier = ctx.label.name)

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
            DefaultInfo(default_output = work),
            RunInfo(args = cmd_args(vite, work)),
        ],
    }

    return [DefaultInfo(default_output = dist, sub_targets = sub_targets)]

vite_bundle = rule(
    impl = _vite_bundle_impl,
    attrs = {
        "srcs": attrs.list(
            attrs.source(),
            doc = "This app's own files (`index.html`, `vite.config.*`, " +
                  "`src/**`), staged at their `short_path` -- so a " +
                  "`vite.config.js` alongside this target's `BUCK` file " +
                  "lands at the work tree root, etc.",
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
    },
)
