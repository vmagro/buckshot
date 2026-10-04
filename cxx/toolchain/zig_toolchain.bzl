"""Hermetic C/C++ toolchain backed by Zig distributions.

`zig cc` / `zig c++` are clang-based compilers that ship their own sysroots
(compiler-rt, musl/mingw libc, macOS SDK headers, libc++) and cross-compile
with a single `-target <triple>` flag, so one downloaded Zig per *host*
covers every *target* triple with no host libc, SDK, or MSVC installation.
This file adapts the prelude's `toolchains/cxx/zig` toolchain (which only
supports one hardcoded host/target pair and an old Zig) to this repo's
conventions:

  - `zig_host_bundle` carries the host-side Zig archive. Its attrs are
    `select(...)` keyed on `buckshot//platforms/configs`, and the toolchain
    references the bundle via `attrs.exec_dep`, so the bundle's analysis
    happens in the *execution* platform and the selects resolve against the
    build host (same pattern as `rust/toolchain/rust_dist.bzl`'s host_bundle).
  - `zig_cxx_toolchain` takes the Zig `-target` triple as a target-side
    `select(...)` string, so one toolchain target cross-compiles from any
    host to macOS, Linux, or Windows.

Use `buck2 run //buckshot -- cxx toolchain` to (re)generate the BUCK
file that wires these rules to specific Zig archives.

Hermeticity notes:

  - Compile, link, archive, ranlib, objcopy, and objdump all run out of the
    downloaded Zig archive. `nm` and `strip` intentionally fall back to
    `$PATH`: Zig ships no `nm`/`strip` subcommands, and neither is invoked
    outside strip/debug-info flows (both disabled here).
  - The wrapper relocates Zig's caches out of the user's home directory:
    the global cache is shared across actions at `<repo>/buck-out/zig-cache`
    (it holds ~50MB of prebuilt CRT/compiler-rt objects per target -- cold
    link 3.5s, warm 0.05s -- so a fresh per-action cache is not an option,
    and content-addressing makes sharing safe), the local cache goes to
    action scratch. Pre-set `ZIG_*_CACHE_DIR` vars are respected. Actions
    need no writable home directory.

Response-file flattening:

  buck2's compile/link actions pass flags via `@argsfile` response files that
  *nest* (the outer file references per-category `@layer` files -- toolchain
  args, deps args, ...). Plain clang expands these recursively, but `zig cc`
  only expands one level and then either errors (`NestedResponseFile`) or
  hands the inner `@file` to clang as an input file. Every `zig <subcommand>`
  therefore goes through the `:zig_tool_wrapper` `python_bootstrap_binary`,
  which structurally flattens nested `@files` (splicing lines verbatim into
  a temp flat file) before invoking Zig, and drops a few exact args Zig's
  bundled linkers reject. It runs on the hermetic bootstrap interpreter, so
  the same wrapper works on mac/linux/windows hosts with no sh, bash, or
  PowerShell involved. See `zig_tool_wrapper.py` for details.
"""

load(
    "@prelude//cxx:cxx_toolchain_types.bzl",
    "AsCompilerInfo",
    "AsmCompilerInfo",
    "BinaryUtilitiesInfo",
    "CCompilerInfo",
    "CxxCompilerInfo",
    "CxxInternalTools",
    "LinkerInfo",
    "LinkerType",
    "PicBehavior",
    "ShlibInterfacesMode",
    "StripFlagsInfo",
    "cxx_toolchain_infos",
)
load("@prelude//cxx:headers.bzl", "HeaderMode")
load("@prelude//cxx:linker.bzl", "is_pdb_generated")
load("@prelude//linking:link_info.bzl", "LinkStyle")

# ==========================================================================
# zig_host_bundle
# ==========================================================================

ZigHostBundleInfo = provider(
    fields = [
        "archive",  # Artifact (unpacked Zig archive root, containing `zig` + `lib/`)
        "os",  # str ("linux", "macos", or "windows") -- the *host* OS
    ],
)

def _zig_host_bundle_impl(ctx):
    if ctx.attrs.os not in ("linux", "macos", "windows"):
        fail("zig_host_bundle os must be one of linux/macos/windows, got: {}".format(ctx.attrs.os))
    return [
        DefaultInfo(),
        ZigHostBundleInfo(
            archive = ctx.attrs.archive[DefaultInfo].default_outputs[0],
            os = ctx.attrs.os,
        ),
    ]

zig_host_bundle = rule(
    attrs = {
        "archive": attrs.dep(doc = "`http_archive` of the Zig distribution for this host. Pass a `select(...)` keyed on host os/cpu."),
        "os": attrs.string(doc = "Host OS the selected archive runs on. Pass a `select(...)` matching `archive`."),
    },
    impl = _zig_host_bundle_impl,
)

# ==========================================================================
# zig_tool command prefix
# ==========================================================================

def _zig_tool(wrapper: RunInfo, zig_exe: Artifact, archive: Artifact, sub: str) -> cmd_args:
    """Prefix for one `zig <sub>` invocation via the flattener wrapper.

    `wrapper` is the `:zig_tool_wrapper` bootstrap binary's `RunInfo`
    (`[hermetic python, zig_tool_wrapper.py]`), which flattens nested
    `@response-files` and drops args Zig rejects before exec'ing
    `zig <sub>` (see `zig_tool_wrapper.py`). The prefix composes into
    larger `cmd_args` (compile/link/archive actions, rust's `-Clinker`
    script, cargo `$CC` shims), so no single-path executable is needed.
    """
    return cmd_args(wrapper, zig_exe, sub, hidden = [archive])

# ==========================================================================
# macos_sdk_shim
# ==========================================================================

def _macos_sdk_shim_impl(ctx):
    out = ctx.actions.declare_output("macos_sdk_shim", dir = True)
    entries = {
        "libiconv.tbd": ctx.attrs.libiconv_tbd[DefaultInfo].default_outputs[0],
    }
    ctx.actions.symlinked_dir(out.as_output(), entries)
    return [DefaultInfo(default_output = out)]

macos_sdk_shim = rule(
    attrs = {
        "libiconv_tbd": attrs.dep(doc = "`export_file` of the minimal link stub for /usr/lib/libiconv.dylib (see macos/libiconv.tbd)."),
    },
    impl = _macos_sdk_shim_impl,
)

# ==========================================================================
# zig_cxx_toolchain
# ==========================================================================

def _zig_target_os(target: str) -> str:
    """OS half of a Zig `-target` triple (`aarch64-macos`, `x86_64-windows-gnu`, `wasm32-freestanding`, ...)."""
    if "wasm32" in target:
        return "wasm"
    if "windows" in target:
        return "windows"
    if "macos" in target:
        return "macos"
    if "linux" in target:
        return "linux"
    fail("cannot determine target OS from Zig triple: {}".format(target))

def _zig_cxx_toolchain_impl(ctx):
    bundle = ctx.attrs.host[ZigHostBundleInfo]
    target_os = _zig_target_os(ctx.attrs.target)

    zig_exe = bundle.archive.project("zig.exe" if bundle.os == "windows" else "zig")
    archive = bundle.archive
    wrapper = ctx.attrs.wrapper[RunInfo]

    zig_cc = _zig_tool(wrapper, zig_exe, archive, "cc")
    zig_cxx = _zig_tool(wrapper, zig_exe, archive, "c++")
    zig_ar = _zig_tool(wrapper, zig_exe, archive, "ar")
    zig_ranlib = _zig_tool(wrapper, zig_exe, archive, "ranlib")
    zig_objcopy = _zig_tool(wrapper, zig_exe, archive, "objcopy")
    zig_objdump = _zig_tool(wrapper, zig_exe, archive, "objdump")
    zig_wasm_ld = _zig_tool(wrapper, zig_exe, archive, "wasm-ld")

    target_flags = ["-target", ctx.attrs.target]

    # Zig's Darwin SDK has no libiconv; without this every macOS link with
    # `-liconv` (i.e. every Rust binary using the `libc` crate) fails. The
    # shim dir only adds a search path, so non-iconv links are unaffected.
    if target_os == "macos":
        shim_dir = ctx.attrs.macos_sdk_shim[DefaultInfo].default_outputs[0]
        target_linker_flags = [cmd_args(shim_dir, format = "-L{}")]
    else:
        target_linker_flags = []

    # macOS uses the Darwin linker flavor: Rust derives proc-macro/cdylib
    # suffixes from it (`.dylib` only under `darwin`; `gnu` would emit
    # `.so`, which rustc rejects), and modern Zig accepts Darwin's
    # `-install_name` fine. The one Darwin flag Zig rejects,
    # `-Wl,-oso_prefix,.`, is filtered out by the wrappers above.
    # Linux and Windows stay GNU-style: `zig cc` speaks the clang GNU
    # driver even when targeting Windows (`-gnu` ABI, `-shared`, `.o`
    # objects), so the MSVC (`/DLL`) flavor would emit flags it does not
    # understand. See also https://github.com/facebook/buck2/issues/470.
    # Wasm gets the first-class `wasm` flavor: the prelude names cdylib
    # outputs `*.wasm` and emits no soname flags for it. rustc drives this
    # target with raw wasm-ld flags (it assumes `-Clinker` is an LLD), so
    # the linker below is `zig wasm-ld` directly rather than `zig c++`
    # (a clang driver, which would reject them).
    if target_os == "macos":
        linker_type = LinkerType("darwin")
    elif target_os == "wasm":
        linker_type = LinkerType("wasm")
    else:
        linker_type = LinkerType("gnu")

    if target_os == "windows":
        binary_extension = "exe"
        shared_library_name_default_prefix = ""
        shared_library_name_format = "{}.dll"
        shared_library_versioned_name_format = "{}.dll"
        pic_behavior = PicBehavior("not_supported")
    elif target_os == "macos":
        binary_extension = ""
        shared_library_name_default_prefix = "lib"
        shared_library_name_format = "{}.dylib"
        shared_library_versioned_name_format = "{}.dylib"
        pic_behavior = PicBehavior("always_enabled")
    elif target_os == "wasm":
        binary_extension = "wasm"
        shared_library_name_default_prefix = ""
        shared_library_name_format = "{}.wasm"
        shared_library_versioned_name_format = "{}.wasm"
        pic_behavior = PicBehavior("supported")
    else:
        binary_extension = ""
        shared_library_name_default_prefix = "lib"
        shared_library_name_format = "{}.so"
        shared_library_versioned_name_format = "{}.so.{}"
        pic_behavior = PicBehavior("supported")

    return [ctx.attrs.host[DefaultInfo]] + cxx_toolchain_infos(
        # Assembly (`.s`/`.S` via `as`, `.asm` via `asm`) assembles through
        # `zig cc` like any other clang toolchain (needed e.g. by `ring`'s
        # per-platform asm blobs). The `-target` flag selects the arch.
        as_compiler_info = AsCompilerInfo(
            compiler = RunInfo(args = cmd_args(zig_cc)),
            compiler_flags = cmd_args(target_flags),
            compiler_type = "clang",
            preprocessor_flags = cmd_args([]),
        ),
        asm_compiler_info = AsmCompilerInfo(
            compiler = RunInfo(args = cmd_args(zig_cc)),
            compiler_flags = cmd_args(target_flags),
            compiler_type = "clang",
            preprocessor_flags = cmd_args([]),
        ),
        binary_utilities_info = BinaryUtilitiesInfo(
            dwp = None,
            # Zig ships no `nm`/`strip`; both fall back to $PATH. Neither is
            # invoked outside strip/debug-info flows, which stay disabled.
            nm = RunInfo(args = ["nm"]),
            objcopy = RunInfo(args = cmd_args(zig_objcopy)),
            objdump = RunInfo(args = cmd_args(zig_objdump)),
            ranlib = RunInfo(args = cmd_args(zig_ranlib)),
            strip = RunInfo(args = ["strip"]),
        ),
        c_compiler_info = CCompilerInfo(
            compiler = RunInfo(args = cmd_args(zig_cc)),
            compiler_flags = cmd_args(target_flags, ctx.attrs.c_compiler_flags),
            compiler_type = "clang",
            preprocessor_flags = cmd_args(ctx.attrs.c_preprocessor_flags),
        ),
        cxx_compiler_info = CxxCompilerInfo(
            compiler = RunInfo(args = cmd_args(zig_cxx)),
            compiler_flags = cmd_args(target_flags, ctx.attrs.cxx_compiler_flags),
            compiler_type = "clang",
            preprocessor_flags = cmd_args(ctx.attrs.cxx_preprocessor_flags),
        ),
        header_mode = HeaderMode("symlink_tree_only"),
        internal_tools = ctx.attrs._cxx_internal_tools[CxxInternalTools],
        linker_info = LinkerInfo(
            archive_objects_locally = False,
            archiver = RunInfo(args = cmd_args(zig_ar)),
            archiver_supports_argfiles = True,
            archiver_type = "gnu",
            binary_extension = binary_extension,
            generate_linker_maps = False,
            independent_shlib_interface_linker_flags = ctx.attrs.shared_library_interface_flags,
            is_pdb_generated = is_pdb_generated(linker_type, ctx.attrs.linker_flags),
            link_binaries_locally = False,
            link_libraries_locally = False,
            link_style = LinkStyle(ctx.attrs.link_style),
            link_weight = 1,
            # `zig wasm-ld` takes no `-target` (nor `-flavor`: rustc emits
            # `-flavor wasm` for its own `rust-lld` dispatch, which the
            # wrapper strips -- see `zig_tool_wrapper.py`).
            linker = RunInfo(args = cmd_args(zig_wasm_ld if target_os == "wasm" else zig_cxx)),
            linker_flags = cmd_args([] if target_os == "wasm" else target_flags, target_linker_flags, ctx.attrs.linker_flags),
            object_file_extension = "o",
            shared_dep_runtime_ld_flags = ctx.attrs.shared_dep_runtime_ld_flags,
            shared_library_name_default_prefix = shared_library_name_default_prefix,
            shared_library_name_format = shared_library_name_format,
            shared_library_versioned_name_format = shared_library_versioned_name_format,
            shlib_interfaces = ShlibInterfacesMode("disabled"),
            static_dep_runtime_ld_flags = ctx.attrs.static_dep_runtime_ld_flags,
            static_library_extension = "a",
            static_pic_dep_runtime_ld_flags = ctx.attrs.static_pic_dep_runtime_ld_flags,
            type = linker_type,
            use_archiver_flags = True,
        ),
        pic_behavior = pic_behavior,
        platform_name = ctx.attrs.target,
        strip_flags_info = StripFlagsInfo(
            strip_all_flags = ctx.attrs.strip_all_flags,
            strip_debug_flags = ctx.attrs.strip_debug_flags,
            strip_non_global_flags = ctx.attrs.strip_non_global_flags,
        ),
    )

zig_cxx_toolchain = rule(
    attrs = {
        "c_compiler_flags": attrs.list(attrs.arg(), default = []),
        "c_preprocessor_flags": attrs.list(attrs.arg(), default = []),
        "cxx_compiler_flags": attrs.list(attrs.arg(), default = []),
        "cxx_preprocessor_flags": attrs.list(attrs.arg(), default = []),
        "host": attrs.exec_dep(
            doc = "`zig_host_bundle` carrying the host's Zig archive. Resolved as exec_dep so the bundle's host-keyed selects fire against the build host (= execution platform).",
            providers = [ZigHostBundleInfo],
        ),
        "link_style": attrs.enum(
            LinkStyle.values(),
            default = "static",
            doc = """
            The default value of the `link_style` attribute for rules that use this toolchain.
            """,
        ),
        "linker_flags": attrs.list(attrs.arg(), default = []),
        "macos_sdk_shim": attrs.exec_dep(
            doc = "`macos_sdk_shim` dir with macOS link stubs missing from Zig's SDK (currently libiconv.tbd). Added via `-L` for macOS targets only.",
            providers = [DefaultInfo],
        ),
        "shared_dep_runtime_ld_flags": attrs.list(attrs.arg(), default = []),
        "shared_library_interface_flags": attrs.list(attrs.string(), default = []),
        "static_dep_runtime_ld_flags": attrs.list(attrs.arg(), default = []),
        "static_pic_dep_runtime_ld_flags": attrs.list(attrs.arg(), default = []),
        "strip_all_flags": attrs.option(attrs.list(attrs.arg()), default = None),
        "strip_debug_flags": attrs.option(attrs.list(attrs.arg()), default = None),
        "strip_non_global_flags": attrs.option(attrs.list(attrs.arg()), default = None),
        "target": attrs.string(
            doc = "Zig `-target` triple for the consumer's target platform (e.g. `aarch64-macos`). Pass a `select(...)` keyed on target os/cpu."
        ),
        "wrapper": attrs.exec_dep(
            default = "buckshot//cxx/toolchain:zig_tool_wrapper",
            doc = "`python_bootstrap_binary` running `zig_tool_wrapper.py` on the hermetic bootstrap interpreter. Prefixes every `zig <sub>` invocation (same binary for all host OSes -- no sh/bash/PowerShell involved).",
            providers = [RunInfo],
        ),
        "_cxx_internal_tools": attrs.default_only(attrs.dep(default = "prelude//cxx/tools:internal_tools", providers = [CxxInternalTools])),
    },
    impl = _zig_cxx_toolchain_impl,
    is_toolchain_rule = True,
)
