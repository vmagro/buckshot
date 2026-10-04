"""Hermetic node toolchain backed by official nodejs.org release archives.

Not a pre-existing buck2/prelude concept (unlike python/rust, which extend
`PythonToolchainInfo`/`RustToolchainInfo`) -- `NodeToolchainInfo` and
`downloaded_node_toolchain` are defined here from scratch. For now the only
thing consumers need is a `node` binary to run npm-package `bin` scripts
with (see `node/node_module.bzl`'s `node_module_providers`), so the
provider carries just that.

Node here is purely a build-time tool (running some npm package's `bin`
script, e.g. `vite`, `tsc`) run by whatever host is doing the
build/executing the action, never shipped as part of a target's own output
for a different platform. `downloaded_node_toolchain` therefore pulls a
`node_host_bundle` in via `attrs.exec_dep`, so the bundle's host-keyed
selects fire against the build host (= execution platform) even when the
toolchain itself is analyzed in an exotic target configuration (e.g.
OS-less wasm32) -- same reasoning as `rust/toolchain/rust_dist.bzl`'s
`host_bundle` and `python/toolchain`'s `python_host_bundle`.

`path_style` covers the one layout difference between platforms: unix
archives unpack to `bin/node`, the Windows zip unpacks straight to
`node.exe` at the archive root.

Use `buck2 run //buckshot -- node toolchain --version vX.Y.Z` to
(re)generate the BUCK file that wires this rule to specific release
archives.
"""

NodeHostBundleInfo = provider(
    fields = [
        "archive",  # Artifact (unpacked node distribution root)
        "path_style",  # str ("unix" or "windows") -- the *host* archive layout
    ],
)

def _node_host_bundle_impl(ctx):
    return [
        DefaultInfo(),
        NodeHostBundleInfo(
            archive = ctx.attrs.archive[DefaultInfo].default_outputs[0],
            path_style = ctx.attrs.path_style,
        ),
    ]

node_host_bundle = rule(
    attrs = {
        "archive": attrs.dep(doc = "`http_archive` of the node distribution for this host. Pass a `select(...)` keyed on host os/cpu."),
        "path_style": attrs.enum(
            ["unix", "windows"],
            doc = "Archive layout for the selected host. Pass a `select(...)` matching `archive`.",
        ),
    },
    impl = _node_host_bundle_impl,
)

NodeToolchainInfo = provider(
    fields = {
        "node": RunInfo,
        "npm": RunInfo,
        "npx": RunInfo,
    },
)

def _downloaded_node_toolchain_impl(ctx):
    bundle = ctx.attrs.host[NodeHostBundleInfo]
    archive = bundle.archive
    if bundle.path_style == "unix":
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
    attrs = {
        "host": attrs.exec_dep(
            doc = "`node_host_bundle` carrying the host's node archive. "
            + "Resolved as exec_dep so the bundle's host-keyed selects "
            + "fire against the build host (= execution platform), even "
            + "when the toolchain itself is analyzed in an exotic target "
            + "configuration (e.g. OS-less wasm32).",
            providers = [NodeHostBundleInfo],
        ),
    },
    impl = _downloaded_node_toolchain_impl,
    is_toolchain_rule = True,
)
