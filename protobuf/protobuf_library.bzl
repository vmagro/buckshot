load("//rust:rust_library.bzl", "rust_library")

ProtobufLibraryInfo = provider(
    fields = [
        "srcs",  # list[Artifact] — direct .proto sources
        "transitive_srcs",  # tset of struct(srcs: list[Artifact]) — own + deps' sources
    ]
)

_ProtoSrcs = transitive_set()

def _proto_sources_manifest_impl(ctx):
    # Produce a small manifest as the default output so `buck2 build`
    # against a protobuf_library has something concrete to materialize.
    manifest = ctx.actions.declare_output(ctx.label.name + ".srcs")
    lines = [s.short_path for s in ctx.attrs.srcs]
    ctx.actions.write(manifest, "\n".join(lines) + "\n")
    return manifest

def _protobuf_library_impl(ctx):
    manifest = _proto_sources_manifest_impl(ctx)

    srcs_tset = ctx.actions.tset(
        _ProtoSrcs,
        children = [d[ProtobufLibraryInfo].transitive_srcs for d in ctx.attrs.deps],
        value = struct(srcs = ctx.attrs.srcs),
    )

    return [
        DefaultInfo(default_output = manifest, other_outputs = ctx.attrs.srcs),
        ProtobufLibraryInfo(
            srcs = ctx.attrs.srcs,
            transitive_srcs = srcs_tset,
        ),
    ]

_protobuf_library = rule(
    attrs = {
        "deps": attrs.list(attrs.dep(providers = [ProtobufLibraryInfo]), default = []),
        "srcs": attrs.list(attrs.source()),
    },
    impl = _protobuf_library_impl,
)

# Shared codegen action: emit a single self-contained lib.rs so the
# consuming rust_library only needs one source artifact (no
# directory-as-source dance). The rust and tonic impls differ only in the
# codegen binary (an attr) plus the tonic `--client` flag.
def _run_codegen(ctx, category, client):
    lib_rs = ctx.actions.declare_output("lib.rs")
    info = ctx.attrs.proto_lib[ProtobufLibraryInfo]

    cmd = cmd_args(
        ctx.attrs._codegen[RunInfo],
        cmd_args(ctx.attrs._protoc[DefaultInfo].default_outputs[0], format = "--protoc={}"),
    )
    cmd.add(cmd_args(lib_rs.as_output(), format = "--out={}"))

    for proto_path, rust_path in ctx.attrs.extern_paths.items():
        cmd.add(cmd_args("--extern-path={}={}".format(proto_path, rust_path)))
    if client:
        cmd.add(cmd_args("--client"))

    # Direct sources are compiled; the whole transitive source closure rides
    # along as import-resolution candidates so the codegen can infer
    # protoc's `--proto-path` roots from the sources' `import` statements
    # instead of spelling a repo-relative directory (which breaks whenever
    # this cell is nested or renamed). Everything on the command line is an
    # artifact, so Buck paths it correctly in any cell layout.
    for s in info.srcs:
        cmd.add(s)
    for batch in info.transitive_srcs.traverse():
        for s in batch.srcs:
            cmd.add(cmd_args(s, format = "--available-src={}"))

    ctx.actions.run(
        cmd,
        category = category,
        identifier = ctx.label.name,
    )
    return [DefaultInfo(default_output = lib_rs)]

def _protobuf_rust_codegen_impl(ctx):
    return _run_codegen(ctx, "proto_rust_codegen", False)

_protobuf_rust_codegen = rule(
    attrs = {
        "extern_paths": attrs.dict(attrs.string(), attrs.string(), default = {}),
        "proto_lib": attrs.dep(providers = [ProtobufLibraryInfo]),
        "_codegen": attrs.exec_dep(default = "buckshot//protobuf:proto_codegen"),
        "_protoc": attrs.exec_dep(default = "buckshot//protobuf/protoc:protoc"),
    },
    impl = _protobuf_rust_codegen_impl,
)

def _protobuf_tonic_codegen_impl(ctx):
    # Same single-lib.rs shape as the rust codegen, but the lib carries
    # prost messages *and* the tonic gRPC server.
    return _run_codegen(ctx, "proto_tonic_codegen", ctx.attrs.client)

_protobuf_tonic_codegen = rule(
    attrs = {
        "client": attrs.bool(default = False),
        "extern_paths": attrs.dict(attrs.string(), attrs.string(), default = {}),
        "proto_lib": attrs.dep(providers = [ProtobufLibraryInfo]),
        "_codegen": attrs.exec_dep(default = "buckshot//protobuf:tonic_codegen"),
        "_protoc": attrs.exec_dep(default = "buckshot//protobuf/protoc:protoc"),
    },
    impl = _protobuf_tonic_codegen_impl,
)

def protobuf_library(
    name,
    srcs,
    deps = [],
    include_deps = [],
    languages = ["rust"],
    rust_deps = [],
    runtime_deps = None,
    tonic_runtime_deps = None,
    extern_paths = {},
    tonic_client = False,
    visibility = ["PUBLIC"],
):
    """Declare a protobuf_library + sibling language library target(s).

    The default-named target (`:<name>`) is the native protobuf_library —
    other `protobuf_library`s can list it in `deps`. `:<name>-rust` is the
    generated rust_library (prost messages); that is what Rust callers
    depend on.

    `runtime_deps` is the prost runtime for `-rust`. It defaults to
    buckshot's own `prost` + `prost-types`, but a downstream repo with
    its own vendored tree MUST override it: prost types only unify when
    every crate in the link shares one `prost` instance, and a
    cross-cell `prost` is a second instance even at the same version.
    `rust_deps` carries any further caller-specific deps.

    `"tonic"` in `languages` additionally emits `:<name>-tonic`: a
    self-contained rust_library with the prost messages *and* the tonic
    gRPC server (server only, no client). `tonic_runtime_deps` is its
    runtime (`prost` + `prost-types` + `tonic` + `tonic-prost`); the same
    downstream-override rule applies. A `-tonic` lib's proto `deps`
    resolve to their `-tonic` siblings, so the whole dep closure must
    enable `tonic`. `-rust` and `-tonic` share a crate name, so one Rust
    target cannot depend on both.

    `extern_paths` maps proto package prefixes to Rust crates
    (`{".google.rpc": "::rpc_status"}`) for message types that live in a
    *different* generated crate (i.e. one of `deps`): each key is passed
    to prost as an `extern_path`, so references are emitted as absolute
    paths into that crate. Well-known `google.protobuf.*` types need no
    entry; they map to `prost-types` automatically. Every `deps` entry
    whose types appear in this library's messages or services needs a
    mapping here (it is harmless to map a dep whose types never appear).

    `tonic_client` additionally generates the tonic gRPC *client* in the
    `-tonic` lib (off by default). Clients exist for round-trip tests;
    servers never need them.

    Import roots are inferred, not configured: the codegen parses each
    source's `import "..."` statements and resolves them against the union
    of `srcs` and every transitive dep's `srcs`, so there is no import root
    to keep in sync with the repo layout (a hardcoded repo-relative root
    breaks whenever this cell is nested or renamed). Every imported file
    must therefore be covered by `deps` or `include_deps`; an uncovered
    import fails the codegen naming the missing path and the fix.

    `languages` may be `[]`: then `:<name>` is import-only (its `srcs`
    are still visible to dependents' codegen, e.g. for well-known types
    that prost maps to `prost-types` instead of compiling) and no Rust
    library is emitted. List import-only layers in `include_deps` rather
    than `deps`: both contribute their `srcs` to this library's codegen,
    but only `deps` become `-rust`/`-tonic` sibling edges on the emitted
    Rust libraries (an import-only layer has no sibling to edge to).
    """
    _protobuf_library(
        name = name,
        deps = deps + include_deps,
        srcs = srcs,
        visibility = visibility,
    )

    codegen = name + "__rust_codegen"
    if "rust" in languages:
        _protobuf_rust_codegen(
            name = codegen,
            extern_paths = extern_paths,
            proto_lib = ":" + name,
            visibility = [],
        )

    tonic_codegen = name + "__tonic_codegen"
    if "tonic" in languages:
        _protobuf_tonic_codegen(
            name = tonic_codegen,
            client = tonic_client,
            extern_paths = extern_paths,
            proto_lib = ":" + name,
            visibility = [],
        )

    if "rust" in languages:
        if runtime_deps == None:
            runtime_deps = [
                "@buckshot//third-party/rust:prost",
                "@buckshot//third-party/rust:prost-types",
            ]
        rust_library(
            name = name + "-rust",
            crate = name.replace("-", "_"),
            crate_root = "lib.rs",
            deps = [d + "-rust" for d in deps] + rust_deps + runtime_deps,
            edition = "2024",
            mapped_srcs = {":" + codegen: "lib.rs"},
            srcs = [],
            unittests = False,
            visibility = visibility,
        )

    if "tonic" in languages:
        if tonic_runtime_deps == None:
            tonic_runtime_deps = [
                "@buckshot//third-party/rust:prost",
                "@buckshot//third-party/rust:prost-types",
                "@buckshot//third-party/rust:tonic",
                "@buckshot//third-party/rust:tonic-prost",
            ]
        rust_library(
            name = name + "-tonic",
            crate = name.replace("-", "_"),
            crate_root = "lib.rs",
            deps = [d + "-tonic" for d in deps] + rust_deps + tonic_runtime_deps,
            edition = "2024",
            mapped_srcs = {":" + tonic_codegen: "lib.rs"},
            srcs = [],
            unittests = False,
            visibility = visibility,
        )
