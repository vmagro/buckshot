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
    ],
)
