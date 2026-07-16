"""Assembles a hermetic *work tree* out of the caller's declared srcs
(nothing ambient leaks in), then shells out to a real
`npm install && npm run <script>` inside it. The install itself still
hits the live npm registry -- that part is not hermetic, hence
`local_only` -- but the inputs and outputs are buck-tracked like any
other action.

Simpler than `node_modules_tree.bzl`'s hermetic path, but non-hermetic:
it hits the npm registry at build time, so the action is `local_only`
and not remote-cacheable. Use this for a project you don't want to (or
can't yet) fully lockfile-pin -- reproducing what a bundler like
vite/webpack actually needs from `node_modules` at *bundle* time is a
much bigger surface than reproducing what npm's own resolver needs, so
most projects start here and migrate specific subtrees to the hermetic
path only where it's worth the precision (e.g. a typecheck step's
`typescript` binary).
"""

_NPM_BUILD_SH = """\
#!/usr/bin/env bash
set -euo pipefail
# Resolve to absolute paths up front; we `cd` into the work dir later so
# relative paths like `$out` would otherwise become wrong.
orig_pwd=$(pwd)
abspath() {
    case "$1" in
        /*) printf '%s\\n' "$1" ;;
        *)  printf '%s/%s\\n' "$orig_pwd" "$1" ;;
    esac
}
work=$(abspath "$1"); out=$(abspath "$2"); script="$3"; build_dir="$4"; shift 4
mkdir -p "$work"
# Remaining args are alternating dst/src pairs.
while [ "$#" -gt 0 ]; do
    dst="$1"; src=$(abspath "$2"); shift 2
    mkdir -p "$work/$(dirname "$dst")"
    cp -R "$src" "$work/$dst"
done
cd "$work"
npm install --no-audit --no-fund --silent
npm run "$script"
mkdir -p "$out"
if [ -d "$work/$build_dir" ]; then
    cp -R "$work/$build_dir/." "$out/"
fi
"""

def _npm_build_impl(ctx):
    out = ctx.actions.declare_output(ctx.attrs.build_dir, dir = True)
    work = ctx.actions.declare_output("work", dir = True)

    shim = ctx.actions.write("npm_build.sh", _NPM_BUILD_SH, is_executable = True)

    cmd = cmd_args(shim)
    cmd.add(work.as_output())
    cmd.add(out.as_output())
    cmd.add(ctx.attrs.script)
    cmd.add(ctx.attrs.build_dir)

    for src in ctx.attrs.srcs:
        cmd.add(src.short_path)
        cmd.add(src)
    for relpath, src in ctx.attrs.mapped_srcs.items():
        cmd.add(relpath)
        cmd.add(src)

    ctx.actions.run(
        cmd,
        category = "npm_build",
        identifier = ctx.label.name,
        # `npm install` hits the network; not a remote-cacheable action.
        local_only = True,
        env = dict(ctx.attrs.env),
    )
    return [DefaultInfo(default_output = out)]

npm_build = rule(
    impl = _npm_build_impl,
    attrs = {
        "srcs": attrs.list(
            attrs.source(),
            doc = "Files staged at their `short_path` inside the work tree.",
        ),
        "mapped_srcs": attrs.dict(
            attrs.string(),
            attrs.source(),
            default = {},
            doc = "Extra inputs staged at the keyed relative path. E.g. " +
                  '`{"vendor/widget": ":widget_pkg"}` drops that directory ' +
                  "under `<work>/vendor/widget/`.",
        ),
        "script": attrs.string(
            default = "build",
            doc = "`npm run <script>` to invoke after install (default: build).",
        ),
        "build_dir": attrs.string(
            default = "dist",
            doc = "Directory inside the work tree to copy out as the rule's " +
                  "output (default: `dist/`).",
        ),
        "env": attrs.dict(attrs.string(), attrs.string(), default = {}),
    },
)
