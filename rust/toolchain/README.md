# rust/toolchain

Hermetic rust toolchain: this directory holds the rule definitions (`rust_dist.bzl`), the helper programs (`*.py`), and the static `BUCK` wiring them together. The toolchain instances themselves are versioned — one generated `BUCK` per release, each alone in its own directory:

- `rust/toolchains/nightly/<date>/BUCK` — dated nightlies (e.g. `rust/toolchains/nightly/2026-07-16`)
- `rust/toolchains/stable/<version>/BUCK` — stables (e.g. `rust/toolchains/stable/1.99.0`)

with rolling aliases in the generated `rust/toolchains/BUCK` (`nightly` = latest nightly, `stable` = latest stable, `rust-lld` tracking `nightly`). `toolchains//:rust` (the default) resolves through the default alias in `rust/toolchain/BUCK`, which points at one of the rolling aliases.

The generator itself lives in `buckshot/` at the repo root (the `rust toolchain` subcommand of buckshot's own CLI — see `buckshot/README.md`) rather than here, because it's a buck2-built `rust_binary`: it can't be colocated with the files it generates without risking clobbering its own build inputs when regenerating them (same reasoning `third-party/npm/`'s generator lives in `buckshot/` instead of alongside `third-party/npm/BUCK` — see `third-party/npm/README.md`).

## Quick start (defaults are pre-configured)

Run with just `--channel-toml` — the generator ships with defaults covering all major platforms, derives the versioned `--output` path from the channel, and refreshes the rolling aliases:

```bash
buck2 run //buckshot -- rust toolchain \
  --channel-toml https://static.rust-lang.org/dist/2026-07-11/channel-rust-nightly.toml
# writes rust/toolchains/nightly/2026-07-11/BUCK + refreshes rust/toolchains/BUCK
```

For a stable release, point at the stable channel (the version comes from the TOML itself):

```bash
buck2 run //buckshot -- rust toolchain \
  --channel-toml https://static.rust-lang.org/dist/channel-rust-stable.toml
# writes rust/toolchains/stable/<version>/BUCK + refreshes rust/toolchains/BUCK
```

Then format the generated files (CI enforces `starlark_fmt` cleanliness):

```bash
./tools/buck/starlark_fmt --config tools/buck/starlark_fmt.json fmt \
  rust/toolchains/nightly/2026-07-11/BUCK rust/toolchains/BUCK
```

## Defaults

| Category   | Values                                                                         |
|------------|--------------------------------------------------------------------------------|
| **Hosts**  | `aarch64-apple-darwin`, `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc` |
| **Targets**| `wasm32-unknown-unknown`, `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-gnu`, `aarch64-apple-darwin`, `x86_64-pc-windows-gnu` |

Windows targets use the GNU ABI (while the windows *host* stays `-msvc`,
the only flavor rustup ships host tools for): rustc links through the cxx
toolchain's `zig c++`, which rejects MSVC-style flags.

## Updating the toolchain

When a new nightly date arrives (check `https://static.rust-lang.org/dist/channel-rust-nightly.toml` for the date):

```bash
DATE=$(curl -s https://static.rust-lang.org/dist/channel-rust-nightly.toml \
  | head -3 | grep '^date' | sed 's/.*"\(.*\)"/\1/')
buck2 run //buckshot -- rust toolchain \
  --channel-toml "https://static.rust-lang.org/dist/$DATE/channel-rust-nightly.toml"
```

This adds a new versioned directory and moves the `nightly` alias to it (the alias always tracks the latest dated release on disk). The previous release's directory stays put, so rolling back is a re-run away (re-running the generator for any remaining release refreshes the aliases from whatever is on disk).

**Careful:** if the generator ever writes a broken `BUCK` file (bad flags, a bug), buck2 can't rebuild *anything* afterward -- including the generator itself -- since `//buckshot:buckshot` needs the default toolchain to build. Prefer generating new releases (a new directory can't break the previous one); if a default-tracked file breaks, recover with `git checkout -- rust/toolchains/` (or `git diff`/`git stash` first if you want to inspect what broke) before trying again. Never pass `--output rust/toolchain/BUCK` (the generator refuses): that file holds the hand-written wiring shared by every release.

## Custom hosts/targets

Override defaults with `--host` / `--target` (repeatable):

```bash
buck2 run //buckshot -- rust toolchain \
  --channel-toml https://static.rust-lang.org/dist/$DATE/channel-rust-nightly.toml \
  --host aarch64-apple-darwin \
  --host x86_64-unknown-linux-gnu \
  --target wasm32-unknown-unknown
```

## What gets generated

Each versioned `rust/toolchains/<channel>/<version>/BUCK` contains:

1. `http_archive` targets for each component (rustc, rust-std, clippy, rustfmt, cargo) per host triple
2. `host_bundle` — selects host components by execution platform via `select()`
3. `rust_lld` — extracts `rust-lld` from the rustc archive for non-toolchain consumers
4. `downloaded_rust_toolchain` — the final toolchain provider with target-side select() for std libraries and triples, wired to the rule definitions in `rust/toolchain/rust_dist.bzl`. It reads the host Zig binary from the cxx toolchain's `zig_host_bundle` for the `dlltool` shims.

The shared pieces live in the static `rust/toolchain/BUCK`, referenced from every instance via the rule defaults: the `rustc_wrapper`, `assemble_sysroot`, and `extract_rust_lld` `python_bootstrap_binary` helpers running on the hermetic bootstrap interpreter (the rustc-family wrapper, which provisions windows-gnu `dlltool` shims; the sysroot assembler; the rust-lld extractor). See `rust/toolchain/*.py`.

`rust/toolchains/BUCK` holds the rolling `nightly` / `stable` / `rust-lld` aliases, also generated (deterministically from the releases on disk — same releases, same bytes). `toolchains/BUCK` exposes the default at the well-known `toolchains//:rust` target via a thin `toolchain_alias` pointing at `rust/toolchain:toolchain`, which itself aliases the rolling `nightly` toolchain (flip it to `:stable` there to change the default).

## Adding a new triple

If you need a triple not in the default mapping, edit the `platform_for` match in `buckshot/src/rust_toolchain/manifest.rs`:

```rust
pub fn platform_for(triple: &str) -> anyhow::Result<(&'static str, Option<&'static str>)> {
    Ok(match triple {
        ...
        "new-triple" => ("cpu_name", Some("os")), // None for platform-less targets like wasm
        ...
    })
}
```
