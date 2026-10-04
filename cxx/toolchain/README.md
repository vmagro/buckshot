# cxx/toolchain

Hermetic cxx toolchain backed by downloaded Zig distributions: this directory holds the rule definitions (`zig_toolchain.bzl`), the wrapper program (`zig_tool_wrapper.py`), and the static `BUCK` wiring them together. The toolchain instances themselves are versioned — one generated `BUCK` per release, each alone in its own directory:

- `cxx/toolchains/<version>/BUCK` — e.g. `cxx/toolchains/0.16.0`

with rolling aliases in the generated `cxx/toolchains/BUCK` (one `<major>.<minor>` alias per minor line, each pointing at the latest patch release, plus `host` — the host-Zig bundle the rust toolchain's `zig_host` exec_dep reads — tracking the latest release overall). `toolchains//:cxx` (the default) resolves through those aliases.

The generator itself lives in `buckshot/` at the repo root (the `cxx toolchain` subcommand of buckshot's own CLI — see `buckshot/README.md`) rather than here, because it's a buck2-built `rust_binary`: it can't be colocated with the files it generates without risking clobbering its own build inputs when regenerating them (same reasoning `third-party/npm/`'s generator lives in `buckshot/` instead of alongside `third-party/npm/BUCK` — see `third-party/npm/README.md`).

`zig cc` / `zig c++` are clang-based compilers that ship their own sysroots (compiler-rt, musl/mingw libc, macOS SDK headers, libc++) and cross-compile with a single `-target <triple>` flag, so one downloaded Zig per *host* covers every *target* triple with no host libc, SDK, or MSVC installation.

## Quick start (defaults are pre-configured)

Run with no flags — the generator defaults to the latest stable Zig and covers all major platforms, derives the versioned `--output` path from the release, and refreshes the rolling aliases:

```bash
buck2 run //buckshot -- cxx toolchain
# writes cxx/toolchains/<version>/BUCK + refreshes cxx/toolchains/BUCK
```

Then format the generated files (CI enforces `starlark_fmt` cleanliness):

```bash
./tools/buck/starlark_fmt --config tools/buck/starlark_fmt.json fmt \
  cxx/toolchains/<version>/BUCK cxx/toolchains/BUCK
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

This adds a new versioned directory and moves the rolling alias to it. The previous release's directory stays put, so rolling back is a re-run away (re-running the generator for any remaining release refreshes the aliases from whatever is on disk).

**Careful:** if the generator ever writes a broken `BUCK` file (bad flags, a bug), buck2 can't rebuild *anything* afterward -- including the generator itself -- since `//buckshot:buckshot` needs the default toolchain (via rustc's `-Clinker`) to build. Prefer generating new releases (a new directory can't break the previous one); if a default-tracked file breaks, recover with `git checkout -- cxx/toolchains/` (or `git diff`/`git stash` first if you want to inspect what broke) before trying again. Never pass `--output cxx/toolchain/BUCK` (the generator refuses): that file holds the hand-written wiring shared by every release.

## Custom hosts/targets

Override defaults with `--host` / `--target` (repeatable, rust-style triples):

```bash
buck2 run //buckshot -- cxx toolchain \
  --host aarch64-apple-darwin \
  --host x86_64-unknown-linux-gnu \
  --target x86_64-pc-windows-gnu
```

## What gets generated

Each versioned `cxx/toolchains/<version>/BUCK` contains:

1. `http_archive` targets, one Zig distribution per host triple
2. `zig_host_bundle` — selects the host Zig archive + host OS string by execution platform via `select()`
3. `zig_cxx_toolchain` — the final toolchain provider with a target-side `target` select mapping each platform config to its `zig -target` triple, wired to the rule definitions in `cxx/toolchain/zig_toolchain.bzl`.

The shared pieces live in the static `cxx/toolchain/BUCK`: `zig_tool_wrapper` — a `python_bootstrap_binary` running `zig_tool_wrapper.py` on the hermetic bootstrap interpreter (it flattens nested `@response-files`, which `zig cc` can't expand recursively, and drops args Zig rejects, identically on every host OS) — and the `libiconv.tbd` + `macos_sdk_shim` pair (Zig's Darwin SDK lacks libiconv, needed by Rust's `libc` crate, so macOS links get this stub dir via `-L`).

`cxx/toolchains/BUCK` holds the rolling `<major>.<minor>` + `host` aliases, also generated (deterministically from the releases on disk — same releases, same bytes). `toolchains/BUCK` exposes the default at the well-known `toolchains//:cxx` target via a thin `toolchain_alias` pointing at `cxx/toolchain:toolchain`, which itself aliases the rolling toolchain.

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
