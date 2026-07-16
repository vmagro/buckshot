# toolchains/rust

Hermetic rust toolchain: this directory holds the generated `BUCK` + the rule definitions it loads (`rust_dist.bzl`). The generator itself lives in `buckshot/` at the repo root (the `rust toolchain` subcommand of buckshot's own CLI — see `buckshot/README.md`) rather than here, because it's a buck2-built `rust_binary`: it can't be colocated with `toolchains/rust/BUCK` without risking clobbering its own build target when regenerating that file (same reasoning `third-party/npm/`'s generator lives in `buckshot/` instead of alongside `third-party/npm/BUCK` — see `third-party/npm/README.md`).

## Quick start (defaults are pre-configured)

Run with just `--channel-toml` — the generator ships with defaults covering all major platforms, and `--output` defaults to `toolchains/rust/BUCK`:

```bash
buck2 run //buckshot -- rust toolchain \
  --channel-toml https://static.rust-lang.org/dist/2026-07-11/channel-rust-nightly.toml
```

## Defaults

| Category   | Values                                                                         |
|------------|--------------------------------------------------------------------------------|
| **Hosts**  | `aarch64-apple-darwin`, `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc` |
| **Targets**| `wasm32-unknown-unknown`, `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-gnu`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc` |

## Updating the toolchain

When a new nightly date arrives (check `https://static.rust-lang.org/dist/channel-rust-nightly.toml` for the date):

```bash
DATE=$(curl -s https://static.rust-lang.org/dist/channel-rust-nightly.toml \
  | head -3 | grep '^date' | sed 's/.*"\(.*\)"/\1/')
buck2 run //buckshot -- rust toolchain \
  --channel-toml "https://static.rust-lang.org/dist/$DATE/channel-rust-nightly.toml"
```

**Careful:** if the generator ever writes a broken `BUCK` file (bad flags, a bug), buck2 can't rebuild *anything* afterward -- including the generator itself -- since `//buckshot:buckshot` needs this same toolchain to build. Recover with `git checkout -- toolchains/rust/BUCK` (or `git diff`/`git stash` first if you want to inspect what broke) before trying again.

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

`toolchains/rust/BUCK` contains:

1. `http_archive` targets for each component (rustc, rust-std, clippy, rustfmt, cargo) per host triple
2. `config_setting` targets mapping OS/cpu constraint combinations
3. `host_bundle` — selects host components by execution platform via `select()`
4. `rust_lld` — extracts `rust-lld` from the rustc archive for non-toolchain consumers
5. `downloaded_rust_toolchain` — the final toolchain provider with target-side select() for std libraries and triples, wired to the rule definitions in `toolchains/rust/rust_dist.bzl`

`toolchains/BUCK` (one level up) exposes it at the well-known `toolchains//:rust` target via a thin `toolchain_alias` pointing at `//rust:rust`.

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
