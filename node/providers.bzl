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
        # dict[str, struct(package_dir, bin)] -- this package plus its own
        # transitive dependency closure, keyed the same way
        # `node_modules_tree`'s own `packages` dict is (see
        # `node/node_modules_tree.bzl`). A plain, already-flattened dict
        # rather than a buck2 transitive_set: tsets can't compose across a
        # cell boundary today (facebook/buck2#683), while a dict is just a
        # value, so copying+merging it at each level (see
        # `node/node_module.bzl`'s `node_module_providers`) works
        # everywhere at the cost of some redundant per-package work.
        "node_modules",
    ],
)
