"""The universal "this artifact looks like an npm package" handle.

Any rule that produces a directory shaped like an npm package
(`package.json` + content) advertises it. Downstream rules
(`npm_build`'s `mapped_srcs`, `node_modules_tree`, etc.) consume it
without poking at DefaultInfo.
"""

JsPackageInfo = provider(
    fields = [
        "package_name",  # str -- npm-style name, e.g. "picocolors"
        "package_dir",  # Artifact -- the directory that `node_modules/<name>` should symlink to
        "bin",  # dict[str, str] -- binname -> path relative to package_dir, for node_modules/.bin
        # TransitiveSet (`NodeModulesTSet`, see `node/node_modules_tree.bzl`)
        # -- this package plus its own dependency closure, as a DAG of
        # placement records rather than a materialized directory. Free to
        # build (no actions run) and automatically deduplicated wherever
        # it's shared, so every package can carry one regardless of
        # whether anything ever needs a real directory from it. Whoever
        # does -- a `bin` script, or a rule composing several packages
        # into one bigger tree -- calls `merge_node_modules_tset` to
        # actually pay for it.
        "node_modules_tset",
    ],
)
