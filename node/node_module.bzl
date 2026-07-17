"""`node_module` -- a single npm-shaped JS package as a buck2 target: some
`srcs` (its own file content -- typically a `package.json` plus
`.js`/`.d.ts` files) plus `deps` on other packages (other `node_module`
targets, or `npm_archive` third-party ones -- anything providing
`JsPackageInfo`).

`third-party/npm/defs.bzl`'s `npm_archive` is built on the exact same
core logic here (`node_module_providers`) -- the only difference is where
`package_dir` comes from: `srcs` assembled into a directory in this file,
an `http_archive` fetch (peeled via `strip_prefix`) there. Everything
past that -- `JsPackageInfo.node_modules`, `bin` -> `RunInfo` sub_targets
-- is shared, so an in-tree, user-authored package and a third-party one
compose the exact same way wherever something wants a `JsPackageInfo`,
across cell boundaries included.

NOTE: this used to propagate each package's closure with a buck2
transitive_set (tset), which is the more natural fit -- one cheap node
per package, shared structure deduplicated for free. That broke as soon
as a tset assembled in one cell (e.g. `tests//...`) needed to merge with
one from another (`//third-party...`): buck2 doesn't support tsets
aggregating across cells (facebook/buck2#683, open/unfixed). Since
cross-cell composition is the whole point of a package like `npm_archive`
living in `third-party` being usable from anywhere, `node_modules` is a
plain `dict[str, struct]` instead, copied and merged with ordinary
Starlark dict operations at each level. That's `O(closure size)` dict
work per package rather than a tset's shared/lazy structure, but dicts
and structs are plain values -- no cross-cell identity problem -- so it
actually works everywhere. Revisit if upstream ever fixes #683.
"""

load(":node_modules_tree.bzl", "build_node_modules_layout")
load(":providers.bzl", "JsPackageInfo")
load("@buckshot//node/toolchain:node_toolchain.bzl", "NodeToolchainInfo")

def node_module_providers(ctx, *, package_name, package_dir, deps, bin, main = None, immutable):
    """Build the `[DefaultInfo, JsPackageInfo]` (+ `RunInfo` if `main` is
    set) that every `JsPackageInfo` producer (see this file's own doc)
    returns, given an already-assembled `package_dir` Artifact.

    `immutable`: see `JsPackageInfo`'s own doc (`providers.bzl`) -- `True`
    for `npm_archive`, `False` for an in-tree `node_module`.

    Every package gets a `node_modules` dict (itself + the union of
    `deps`' own already-flattened dicts) -- pure Starlark dict copying,
    no actions run. `bin` (name -> path relative to `package_dir`,
    matching `package.json#bin`) each get a `RunInfo` sub_target that
    runs from inside a real merged `node_modules/` tree (only built, via
    `build_node_modules_layout`, when `bin` is non-empty) -- e.g.
    `typescript`'s `tsc` resolves a platform-specific optional dependency
    via Node's own module resolution at runtime, which needs a real
    `node_modules` ancestor directory, not just a `NODE_PATH` env var
    (ESM's `import.meta.resolve` doesn't consult `NODE_PATH`).

    `main`, if set, is a path (relative to `package_dir`) merged into
    `bin` under this package's own name -- mirroring npm's `package.json`
    shorthand where a plain string `bin` field (instead of a dict) means
    "one binary, named after the package". Its `RunInfo` is *also*
    returned as a top-level provider (not just on a `[name]` sub_target),
    so `buck2 run :name` runs it directly.

    Every `bin` script is run through the hermetic `node/toolchain`
    binary (`ctx.attrs._node_toolchain`), never a system-installed
    `node` on `$PATH` -- so both callers of this function (`_node_module`
    below, `third-party/npm/defs.bzl`'s `_npm_archive`) must declare a
    `"_node_toolchain": attrs.toolchain_dep(default = "toolchains//:node",
    providers = [NodeToolchainInfo])` attr.
    """
    bin = dict(bin)
    if main != None:
        bin[package_name] = main

    node_modules = {}
    for dep in deps:
        node_modules.update(dep[JsPackageInfo].node_modules)
    node_modules[package_name] = struct(package_dir = package_dir, bin = bin, immutable = immutable)

    sub_targets = {}
    main_run_info = None
    if bin:
        tree = build_node_modules_layout(ctx, "modules", node_modules)
        modules_dir = tree.project("node_modules")
        node = ctx.attrs._node_toolchain[NodeToolchainInfo].node
        for bin_name, bin_relpath in bin.items():
            bin_relpath = bin_relpath.removeprefix("./")
            bin_artifact = modules_dir.project(package_name + "/" + bin_relpath)
            run_info = RunInfo(args = cmd_args(
                "env",
                cmd_args(modules_dir, format = "NODE_PATH={}"),
                node,
                bin_artifact,
            ))
            sub_targets[bin_name] = [
                DefaultInfo(default_output = bin_artifact),
                run_info,
            ]
            if bin_name == package_name and main != None:
                main_run_info = run_info

    providers = [
        DefaultInfo(default_output = package_dir, sub_targets = sub_targets),
        JsPackageInfo(
            package_name = package_name,
            package_dir = package_dir,
            bin = bin,
            immutable = immutable,
            node_modules = node_modules,
        ),
    ]
    if main_run_info != None:
        providers.append(main_run_info)
    return providers

def _node_module_impl(ctx):
    package_dir = ctx.actions.symlinked_dir(
        "package",
        {src.short_path: src for src in ctx.attrs.srcs},
    )
    return node_module_providers(
        ctx,
        package_name = ctx.attrs.package_name,
        package_dir = package_dir,
        deps = ctx.attrs.deps,
        bin = ctx.attrs.bin,
        main = ctx.attrs.main,
        # In-tree, user-authored -- `package_dir` above is a
        # `symlinked_dir` straight to checked-in `srcs`, editable at any
        # time. See `JsPackageInfo.immutable`'s own doc.
        immutable = False,
    )

_node_module = rule(
    impl = _node_module_impl,
    attrs = {
        "package_name": attrs.string(
            doc = "npm-style package name, e.g. `@babel/core`.",
        ),
        "srcs": attrs.list(
            attrs.source(),
            default = [],
            doc = "This package's own files, staged at their `short_path` " +
                  "-- so a `package.json` alongside this target's `BUCK` " +
                  "file lands at the package root, etc.",
        ),
        "deps": attrs.list(
            attrs.dep(providers = [JsPackageInfo]),
            default = [],
            doc = "Other packages (`node_module` or `npm_archive` targets, " +
                  "from any cell) this one actually `require()`s/`import`s " +
                  "at runtime.",
        ),
        "bin": attrs.dict(
            attrs.string(),
            attrs.string(),
            default = {},
            doc = "binname -> path (relative to the package dir), matching " +
                  "`package.json#bin` -- same shape as `npm_archive`'s " +
                  "`bin` attr. Each gets its own `RunInfo` sub_target, " +
                  "runnable with `buck2 run :name[binname]`.",
        ),
        "main": attrs.option(
            attrs.string(),
            default = None,
            doc = "Path (relative to the package dir) merged into `bin` " +
                  "under this package's own name -- mirroring npm's " +
                  "`package.json` shorthand where a plain string `bin` " +
                  "field means \"one binary, named after the package\". " +
                  "Its `RunInfo` is also returned at the top level, so " +
                  "`buck2 run :name` (no `[binname]`) runs it directly.",
        ),
        "_node_toolchain": attrs.toolchain_dep(
            default = "toolchains//:node",
            providers = [NodeToolchainInfo],
        ),
    },
)

def node_module(name, package_name = None, srcs = [], deps = [], bin = {}, main = None, visibility = ["PUBLIC"]):
    """In-tree, user-authored equivalent of `npm_archive` -- see this
    file's own doc. `package_name` defaults to `name`, true for every
    plain, unscoped package -- only a scoped name like `@babel/core`
    needs to pass it explicitly.
    """
    _node_module(
        name = name,
        package_name = package_name or name,
        srcs = srcs,
        deps = deps,
        bin = bin,
        main = main,
        visibility = visibility,
    )
