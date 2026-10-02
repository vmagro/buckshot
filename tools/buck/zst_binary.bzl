"""A runnable binary downloaded as a zstd-compressed single file.

`facebook/buck2` GitHub releases publish host tools (`starlark_fmt`,
`rust-project`) only as raw `.zst` files, which `http_archive` cannot unpack
(it expects tar/zip containers) and `http_file` would leave compressed. This
rule downloads the `.zst` for the target platform and decompresses it with
the repo's hermetic CPython (`compression.zstd`, stdlib since 3.14), so no
system `zstd`, dotslash, or anything outside `buck-out` is required.
"""

load("@prelude//os_lookup:defs.bzl", "Os", "OsLookup")
load("@prelude//python:toolchain.bzl", "PythonToolchainInfo")

_UNZSTD_PY = """\
import compression.zstd
import os
import sys

src, dst = sys.argv[1], sys.argv[2]
with open(src, "rb") as f:
    data = compression.zstd.decompress(f.read())
with open(dst, "wb") as f:
    f.write(data)
os.chmod(dst, 0o755)
"""

def _zst_binary_impl(ctx: AnalysisContext) -> list[Provider]:
    url, sha256 = ctx.attrs.url_sha256
    compressed = ctx.actions.declare_output(ctx.label.name + ".zst")
    ctx.actions.download_file(
        compressed.as_output(),
        url,
        sha256 = sha256,
    )

    out_name = ctx.attrs.out or ctx.label.name
    if ctx.attrs._target_os_type[OsLookup].os == Os("windows"):
        out_name += ".exe"
    out = ctx.actions.declare_output(out_name)

    script = ctx.actions.write(
        ctx.label.name + "_unzstd.py",
        _UNZSTD_PY,
    )
    python = ctx.attrs._python_toolchain[PythonToolchainInfo].interpreter
    ctx.actions.run(
        cmd_args(
            python,
            script,
            compressed,
            out.as_output(),
        ),
        category = "unzstd",
    )

    return [
        DefaultInfo(default_output = out),
        RunInfo(args = [out]),
    ]

zst_binary = rule(
    attrs = {
        "out": attrs.option(attrs.string(), default = None),
        "url_sha256": attrs.tuple(attrs.string(), attrs.string()),
        "_python_toolchain": attrs.default_only(
            attrs.toolchain_dep(
                default = "toolchains//:python",
                providers = [PythonToolchainInfo],
            )
        ),
        "_target_os_type": attrs.default_only(
            attrs.dep(
                default = "prelude//os_lookup/targets:os_lookup",
            )
        ),
    },
    impl = _zst_binary_impl,
)
