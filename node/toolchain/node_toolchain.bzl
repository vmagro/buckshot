"""Hermetic node toolchain backed by official nodejs.org release archives.

Not a pre-existing buck2/prelude concept (unlike python/rust, which extend
`PythonToolchainInfo`/`RustToolchainInfo`) -- `NodeToolchainInfo` and
`downloaded_node_toolchain` are defined here from scratch. For now the only
thing consumers need is a `node` binary to run npm-package `bin` scripts
with (see `node/node_module.bzl`'s `node_module_providers`), so the
provider carries just that.

`archive` is an `attrs.exec_dep`: node here is purely a build-time tool
(running some npm package's `bin` script, e.g. `vite`, `tsc`) run by
whatever host is doing the build/executing the action, never shipped as
part of a target's own output for a different platform -- same reasoning
as `rust/toolchain/rust_dist.bzl`'s `host_bundle` and `python/toolchain`'s
`astral_python.bzl` `archive` attr.

`bin_relpath` covers the one layout difference between platforms: unix
archives unpack to `bin/node`, the Windows zip unpacks straight to
`node.exe` at the archive root.

Use `buck2 run //buckshot -- node toolchain --version vX.Y.Z` to
(re)generate the BUCK file that wires this rule to specific release
archives.
"""

NodeToolchainInfo = provider(
    fields = {
        "node": RunInfo,
        "npm": RunInfo,
        "npx": RunInfo,
    },
)

def _downloaded_node_toolchain_impl(ctx):
    archive = ctx.attrs.archive[DefaultInfo].default_outputs[0]
    if ctx.attrs.path_style == "unix":
        node = archive.project("bin/node")
        npm = archive.project("bin/npm")
        npx = archive.project("bin/npx")
    else:
        node = archive.project("node.exe")
        npm = archive.project("npm.cmd")
        npx = archive.project("npx.cmd")
    return [
        DefaultInfo(),
        NodeToolchainInfo(
            node = RunInfo(cmd_args(node)),
            npm = RunInfo(cmd_args(npm)),
            npx = RunInfo(cmd_args(npx)),
        ),
    ]

downloaded_node_toolchain = rule(
    impl = _downloaded_node_toolchain_impl,
    attrs = {
        "archive": attrs.exec_dep(
            providers = [DefaultInfo],
            doc = "http_archive of the unpacked node release for the " +
                  "execution platform -- resolved as exec_dep so the " +
                  "select() picking it fires against the build host, " +
                  "not whatever platform the depending target itself is " +
                  "being built for.",
        ),
        "path_style": attrs.enum(
            ["unix", "windows"],
            default = select({
                "DEFAULT": "unix",
                "prelude//os:windows": "windows",
            })
        ),
    },
    is_toolchain_rule = True,
)
