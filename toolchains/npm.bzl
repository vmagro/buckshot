"""Generic rules for building npm-based JS/TS projects with buck2.

Ported from a downstream project's in-tree wasm+npm dashboard pipeline,
stripped down to the pieces that are useful for any npm project (the
wasm/rust-specific glue and app-specific bundle logic were left behind).

Two complementary mechanisms for getting a `node_modules/` tree:

  - `node_modules_tree` (+ `build_node_modules_layout`) assembles a real
    `node_modules/` directory out of buck-fetched `JsPackageInfo`
    packages (see `third-party/npm/defs.bzl`'s `npm_archive`, generated
    by `buckshot npm buckify`). This is the hermetic path — no network
    access at build time, every tarball pinned by sha256, the npm
    equivalent of what reindeer's vendored crates are for Rust.

  - `npm_build` shells out to a real `npm install && npm run <script>`
    inside a hermetic *work tree* (only the caller's declared srcs are
    staged in). Simpler, but non-hermetic: it hits the npm registry at
    build time, so the action is `local_only` and not remote-cacheable.
    Use this for a project you don't want to (or can't yet) fully
    lockfile-pin — reproducing what a bundler like vite/webpack actually
    needs from `node_modules` at *bundle* time is a much bigger surface
    than reproducing what npm's own resolver needs, so most projects
    start here and migrate specific subtrees to the hermetic path only
    where it's worth the precision (e.g. a typecheck step's `typescript`
    binary).

`npm_install` is a small helper the hermetic path can use to materialize
a `node_modules/` from a single `package.json` (e.g. to get `typescript`
itself) — still non-hermetic (network), but self-contained.
"""

# ==========================================================================
# JsPackageInfo
# ==========================================================================
#
# The universal "this artifact looks like an npm package" handle. Any
# rule that produces a directory shaped like an npm package
# (`package.json` + content) advertises it. Downstream rules
# (`npm_build`'s `mapped_srcs`, `node_modules_tree`, etc.) consume it
# without poking at DefaultInfo.

JsPackageInfo = provider(
    fields = [
        "package_name",  # str -- npm-style name, e.g. "picocolors"
        "package_dir",  # Artifact -- the directory that `node_modules/<name>` should symlink to
        "bin",  # dict[str, str] -- binname -> path relative to package_dir, for node_modules/.bin
    ],
)

# ==========================================================================
# npm_install
# ==========================================================================
#
# Run `npm install` against a `package.json` and capture the resulting
# `node_modules/` as the rule's output. Handy for pulling in a single
# tool (e.g. `typescript`) without hand-writing a `node_modules_tree`.

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

# ==========================================================================
# node_modules_tree / build_node_modules_layout
# ==========================================================================
#
# `packages` keys are exactly the `package-lock.json` `packages` map keys
# with the single leading `node_modules/` stripped (nested overrides keep
# their own embedded `node_modules/...` segments, e.g.
# `escodegen/node_modules/estraverse`). That's deliberate: npm's lockfile
# has already resolved hoisting, so mirroring its keys verbatim as
# directory paths and letting Node's own runtime resolver walk up the
# tree is correct by construction -- no dependency-resolution logic
# needs reimplementing here.
#
# The one wrinkle: a package that has deeper overrides nested inside it
# (e.g. `escodegen` has `escodegen/node_modules/estraverse` beneath it)
# can't just be placed whole at its own path -- something has to be able
# to land *inside* it afterward. `NODE_MODULES_TREE_PY` handles this by
# recursively mirroring each package as real directories with individual
# files *hard-linked* (not symlinked) to the fetched archive, processed
# shallowest-first, so a deeper override can freely replace whatever
# subpath it needs to.
#
# Hard links instead of symlinks is deliberate, not just an optimization:
# some bundlers (e.g. vite) resolve bare imports with symlink
# preservation hardcoded off, so they fully realpath every package they
# touch. A symlink straight at a fetched tarball's buck-out location
# realpaths *out of the tree entirely*, losing every `node_modules`
# ancestor a subsequent bare import needs to walk up through. A hard
# link has no separate "target" for realpath to chase, so the tree keeps
# its own shape no matter how aggressively something upstream
# normalizes paths.
#
# The same script also underlies `build_node_modules_layout`'s `base`
# param (a synthetic `"."` path meaning "mirror this dir's own content
# directly into the node_modules root") -- used to layer extra packages
# on top of an existing prebuilt third-party tree.

NODE_MODULES_TREE_PY = """\
import os
import shutil
import sys


def mirror(src, dst):
    # Recursively recreates `src` at `dst`: real directories, individual
    # files hard-linked (not symlinked or copied) to the original.
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
    pkgs = []
    i = 0
    while i < len(rest) and rest[i] != "--bin--":
        pkgs.append((rest[i], rest[i + 1]))
        i += 2
    bins = []
    if i < len(rest):
        i += 1
        while i < len(rest):
            bins.append((rest[i], rest[i + 1], rest[i + 2]))
            i += 3

    # Hard-link targets, like symlink targets, are resolved relative to
    # the cwd this script runs from unless absolute -- abspath up front.
    path_to_real = {relpath: os.path.abspath(real) for relpath, real in pkgs}

    root = os.path.join(out, "node_modules")
    os.makedirs(root, exist_ok=True)

    # "." is a synthetic path meaning "mirror this dir's own content
    # directly into the node_modules root" rather than "place this whole
    # dir at node_modules/.". Lets callers layer extra packages on top of
    # an existing prebuilt node_modules dir (passed as the "." entry).
    if "." in path_to_real:
        mirror(path_to_real["."], root)

    paths = [p for p in path_to_real if p != "."]

    for relpath in sorted(paths, key=lambda p: p.count("/")):
        dest = os.path.join(root, relpath)
        # A shallower entry's mirror() may have already recreated this
        # exact subpath (e.g. the base "." merge, or an ancestor package
        # that doesn't know it's about to be overridden below itself) --
        # clear it first so a deeper override fully replaces it.
        if os.path.lexists(dest):
            if os.path.isdir(dest) and not os.path.islink(dest):
                shutil.rmtree(dest)
            else:
                os.remove(dest)
        os.makedirs(os.path.dirname(dest), exist_ok=True)
        mirror(path_to_real[relpath], dest)

    if bins:
        bindir = os.path.join(root, ".bin")
        os.makedirs(bindir, exist_ok=True)
        for pkg_relpath, binname, binrelpath in bins:
            # Hard-linked (not symlinked) for the same realpath-transparency
            # reason as everything else in this tree.
            target = os.path.join(root, pkg_relpath, binrelpath)
            os.link(target, os.path.join(bindir, binname))


if __name__ == "__main__":
    main()
"""

def build_node_modules_layout(ctx, name, packages, base = None):
    """Assemble a `<name>/node_modules/` dir and return its Artifact.

    `packages`: dict[str, JsPackageInfo] keyed by node_modules-relative
    path (see this file's own doc for the path convention).
    `base`: optional dep (providing `DefaultInfo`) whose default output is
    an existing `<...>/node_modules/` dir (e.g. a `node_modules_tree`
    target) to shallow-merge at the root -- used to layer `packages` on
    top of a prebuilt third-party tree.
    """
    out = ctx.actions.declare_output(name, dir = True)
    script = ctx.actions.write(name + ".py", NODE_MODULES_TREE_PY)

    cmd = cmd_args("python3", script, out.as_output())
    if base != None:
        cmd.add(".")
        cmd.add(base[DefaultInfo].default_outputs[0].project("node_modules"))

    # Top-level (non-nested) packages get `.bin` entries; collected
    # separately and appended after a `--bin--` sentinel.
    bin_args = []
    for relpath in sorted(packages.keys()):
        info = packages[relpath]
        cmd.add(relpath)
        cmd.add(info.package_dir)
        if "/node_modules/" not in relpath:
            for binname, binrelpath in info.bin.items():
                bin_args.extend([relpath, binname, binrelpath])
    if bin_args:
        cmd.add("--bin--")
        for arg in bin_args:
            cmd.add(arg)

    ctx.actions.run(cmd, category = "node_modules_tree", identifier = name)
    return out

def _node_modules_tree_impl(ctx):
    packages = {relpath: dep[JsPackageInfo] for relpath, dep in ctx.attrs.packages.items()}
    out = build_node_modules_layout(ctx, "node_modules_tree", packages)
    return [DefaultInfo(default_output = out)]

node_modules_tree = rule(
    impl = _node_modules_tree_impl,
    attrs = {
        "packages": attrs.dict(
            attrs.string(),
            attrs.dep(providers = [JsPackageInfo]),
            default = {},
            doc = "Map of node_modules-relative path (leading `node_modules/` " +
                  "stripped once; nested overrides keep embedded " +
                  "`node_modules/...` segments) to the `npm_archive` (or other " +
                  "`JsPackageInfo`) target providing that path's content.",
        ),
    },
)

# ==========================================================================
# npm_build
# ==========================================================================
#
# Assembles a hermetic *work tree* out of the caller's declared srcs
# (nothing ambient leaks in), then shells out to a real
# `npm install && npm run <script>` inside it. The install itself still
# hits the live npm registry -- that part is not hermetic, hence
# `local_only` -- but the inputs and outputs are buck-tracked like any
# other action.

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
