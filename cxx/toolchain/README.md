# cxx/toolchain

Hermetic cxx toolchain backed by downloaded Zig distributions: this directory holds the generated `BUCK` + the rule definitions it loads (`zig_toolchain.bzl`) and the wrapper program (`zig_tool_wrapper.py`). The generator itself lives in `buckshot/` at the repo root (the `cxx toolchain` subcommand of buckshot's own CLI — see `buckshot/README.md`) rather than here, because it's a buck2-built `rust_binary`: it can't be colocated with `cxx/toolchain/BUCK` without risking clobbering its own build target when regenerating that file (same reasoning `third-party/npm/`'s generator lives in `buckshot/` instead of alongside `third-party/npm/BUCK` — see `third-party/npm/README.md`).

`zig cc` / `zig c++` are clang-based compilers that ship their own sysroots (compiler-rt, musl/mingw libc, macOS SDK headers, libc++) and cross-compile with a single `-target <triple>` flag, so one downloaded Zig per *host* covers every *target* triple with no host libc, SDK, or MSVC installation.

## Quick start (defaults are pre-configured)

Run with no flags — the generator defaults to the latest stable Zig and covers all major platforms, and `--output` defaults to `cxx/toolchain/BUCK`:

```bash
buck2 run //buckshot -- cxx toolchain
```

## Defaults

| Category   | Values                                                                         |
|------------|--------------------------------------------------------------------------------|
| **Hosts**  | `aarch64-apple-darwin`, `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc` |
| **Targets**| same as hosts (every host doubles as a target)                                 |

Windows targets use the GNU ABI (`x86_64-windows-gnu`): `zig c++` rejects MSVC-style flags, so the toolchain links windows-gnu (same as `cargo-zigbuild`).

## Updating the toolchain

```bash
buck2 run //buckshot -- cxx toolchain --version 0.17.0
```

(available versions: https://ziglang.org/download/index.json; omit `--version` for the latest stable.)

**Careful:** if the generator ever writes a broken `BUCK` file (bad flags, a bug), buck2 can't rebuild *anything* afterward -- including the generator itself -- since `//buckshot:buckshot` needs this same toolchain (via rustc's `-Clinker`) to build. Recover with `git checkout -- cxx/toolchain/BUCK` (or `git diff`/`git stash` first if you want to inspect what broke) before trying again.

## Custom hosts/targets

Override defaults with `--host` / `--target` (repeatable, rust-style triples):

```bash
buck2 run //buckshot -- cxx toolchain \
  --host aarch64-apple-darwin \
  --host x86_64-unknown-linux-gnu \
  --target x86_64-pc-windows-gnu
```

## What gets generated

`cxx/toolchain/BUCK` contains:

1. `zig_tool_wrapper` — a `python_bootstrap_binary` running `zig_tool_wrapper.py` on the hermetic bootstrap interpreter. It flattens nested `@response-files` (which `zig cc` can't expand recursively) and drops args Zig rejects, identically on every host OS.
2. `http_archive` targets, one Zig distribution per host triple
3. `zig_host_bundle` — selects the host Zig archive + host OS string by execution platform via `select()`
4. `libiconv.tbd` + `macos_sdk_shim` — Zig's Darwin SDK lacks libiconv (needed by Rust's `libc` crate), so macOS links get this stub dir via `-L`
5. `zig_cxx_toolchain` — the final toolchain provider with a target-side `target` select mapping each platform config to its `zig -target` triple, wired to the rule definitions in `cxx/toolchain/zig_toolchain.bzl`.

`toolchains/BUCK` exposes it at the well-known `toolchains//:cxx` target via a thin `toolchain_alias` pointing at `buckshot//cxx/toolchain:toolchain`.

## Adding a new triple

If you need a triple not in the default mapping:

- `zig_arch_for` in `buckshot/src/cxx_toolchain/manifest.rs` maps host triples to the index's arch names (`aarch64-macos`, `x86_64-windows`, ...),
- `platform_for` maps triples to `(cpu, os)` config labels,
- `zig_target_for` maps `(cpu, os)` to the `zig -target` triple.

```rust
"new-triple" => ("cpu_name", "os"), // platform_for
```

## Host/target support matrix

The toolchain itself (compile, link, archive via `zig cc`/`zig c++`) works from all three host OSes to all three target OSes. Adjacent gaps live in the *consumers*, not here:

- windows-*host* rust builds of `raw-dylib` crates (e.g. anything via `windows-sys`) need a `dlltool` rustc can spawn; the `:rustc_wrapper` shims are POSIX-only (see `rust/toolchain/rust_dist.bzl`).
- `nm`/`strip` fall back to `$PATH` (Zig ships neither; both are outside strip/debug-info flows, which stay disabled).
