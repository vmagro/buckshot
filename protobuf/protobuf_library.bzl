load("//rust:rust_library.bzl", "rust_library")

ProtobufLibraryInfo = provider(
    fields = [
        "srcs",  # list[Artifact] — direct .proto sources
        "proto_root",  # string — import root for `srcs` (relative to package dir)
        "transitive_includes",  # tset of struct(root: str, srcs: list[Artifact])
    ]
)

_ProtoIncludes = transitive_set()

def _proto_sources_manifest_impl(ctx):
    # Produce a small manifest as the default output so `buck2 build`
    # against a protobuf_library has something concrete to materialize.
    manifest = ctx.actions.declare_output(ctx.label.name + ".srcs")
    lines = [s.short_path for s in ctx.attrs.srcs]
    ctx.actions.write(manifest, "\n".join(lines) + "\n")
    return manifest

def _protobuf_library_impl(ctx):
    manifest = _proto_sources_manifest_impl(ctx)

    inc_tset = ctx.actions.tset(
        _ProtoIncludes,
        children = [d[ProtobufLibraryInfo].transitive_includes for d in ctx.attrs.deps],
        value = struct(root = ctx.attrs.proto_root, srcs = ctx.attrs.srcs),
    )

    return [
        DefaultInfo(default_output = manifest, other_outputs = ctx.attrs.srcs),
        ProtobufLibraryInfo(
            proto_root = ctx.attrs.proto_root,
            srcs = ctx.attrs.srcs,
            transitive_includes = inc_tset,
        ),
    ]

_protobuf_library = rule(
    attrs = {
        "deps": attrs.list(attrs.dep(providers = [ProtobufLibraryInfo]), default = []),
        "proto_root": attrs.string(default = "."),
        "srcs": attrs.list(attrs.source()),
    },
    impl = _protobuf_library_impl,
)

def _protobuf_rust_codegen_impl(ctx):
    # Emit a single self-contained lib.rs so the consuming rust_library
    # only needs one source artifact (no directory-as-source dance).
    lib_rs = ctx.actions.declare_output("lib.rs")
    info = ctx.attrs.proto_lib[ProtobufLibraryInfo]

    cmd = cmd_args(
        ctx.attrs._codegen[RunInfo],
        cmd_args(ctx.attrs._protoc[DefaultInfo].default_outputs[0], format = "--protoc={}"),
    )
    cmd.add(cmd_args(lib_rs.as_output(), format = "--out={}"))

    seen_roots = {}
    hidden_inputs = []
    for inc in info.transitive_includes.traverse():
        if inc.root not in seen_roots:
            cmd.add(cmd_args(inc.root, format = "--proto-path={}"))
            seen_roots[inc.root] = True
        for s in inc.srcs:
            hidden_inputs.append(s)

    for s in info.srcs:
        cmd.add(s)
    cmd.add(cmd_args(hidden = hidden_inputs))

    ctx.actions.run(
        cmd,
        category = "proto_rust_codegen",
        identifier = ctx.label.name,
    )
    return [DefaultInfo(default_output = lib_rs)]

_protobuf_rust_codegen = rule(
    attrs = {
        "proto_lib": attrs.dep(providers = [ProtobufLibraryInfo]),
        "_codegen": attrs.exec_dep(default = "buckshot//protobuf:proto_codegen"),
        "_protoc": attrs.exec_dep(default = "buckshot//protobuf/protoc:protoc"),
    },
    impl = _protobuf_rust_codegen_impl,
)

def protobuf_library(
    name,
    srcs,
    proto_root = ".",
    deps = [],
    languages = ["rust"],
    visibility = ["PUBLIC"],
):
    """Declare a protobuf_library + sibling language library target(s).

    The default-named target (`:<name>`) is the native protobuf_library —
    other `protobuf_library`s can list it in `deps`.
    """
    _protobuf_library(
        name = name,
        deps = deps,
        proto_root = proto_root,
        srcs = srcs,
        visibility = visibility,
    )

    codegen = name + "__rust_codegen"
    _protobuf_rust_codegen(
        name = codegen,
        proto_lib = ":" + name,
        visibility = [],
    )

    if "rust" in languages:
        rust_library(
            name = name + "-rust",
            crate = name.replace("-", "_"),
            crate_root = "lib.rs",
            deps = [d + "-rust" for d in deps]
            + [
                # TODO: we probably can't just blindly use these copies of these
                # crates, because the consumer project might have their own
                # versions. Punt to figuring that out later
                "@buckshot//third-party/rust:prost",
                "@buckshot//third-party/rust:prost-types",
            ],
            edition = "2024",
            mapped_srcs = {":" + codegen: "lib.rs"},
            srcs = [],
            unittests = False,
            visibility = visibility,
        )
