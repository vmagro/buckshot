"""Run `npm install` against a `package.json` and capture the resulting
`node_modules/` as the rule's output. Handy for pulling in a single
tool (e.g. `typescript`) without hand-writing a `node_modules_tree`.

Still non-hermetic (network), but self-contained.
"""

_NPM_INSTALL_SH = """\
#!/usr/bin/env bash
set -euo pipefail
orig_pwd=$(pwd)
abspath() { case "$1" in /*) echo "$1" ;; *) echo "$orig_pwd/$1" ;; esac }
out=$(abspath "$1"); pkg=$(abspath "$2")
work=$(mktemp -d)
cp "$pkg" "$work/package.json"
cd "$work"
npm install --no-audit --no-fund --silent
mkdir -p "$out"
# Move both visible and dot-prefixed (e.g. .bin/) entries.
for f in node_modules/* node_modules/.*; do
    [ -e "$f" ] || continue
    base=$(basename "$f")
    case "$base" in .|..) continue ;; esac
    mv "$f" "$out/"
done
"""

def _npm_install_impl(ctx):
    out = ctx.actions.declare_output("node_modules", dir = True)
    shim = ctx.actions.write("npm_install.sh", _NPM_INSTALL_SH, is_executable = True)
    cmd = cmd_args(shim, out.as_output(), ctx.attrs.package_json)
    ctx.actions.run(
        cmd,
        category = "npm_install",
        identifier = ctx.label.name,
        # Hits the network; not a remote-cacheable action.
        local_only = True,
    )
    return [DefaultInfo(default_output = out)]

npm_install = rule(
    impl = _npm_install_impl,
    attrs = {
        "package_json": attrs.source(
            doc = "package.json whose `dependencies`/`devDependencies` should " +
                  "be installed. Output is the resulting `node_modules/` dir.",
        ),
    },
)
