# gen_rust_toolchain

Generate a Buck2 `downloaded_rust_toolchain` BUCK file from a rustup release-channel TOML. Lives alongside the files it reads/writes (`BUCK`, `rust_dist.bzl`) — same colocation convention as `third-party/npm/npm_buckify`. Stays a plain script (rather than a buck2-built rust_binary like `npm_buckify` now is) because it generates the very toolchain a rust_binary would need in order to build -- it has to run before this repo has a rust toolchain to build it with.

## Quick start (defaults are pre-configured)

Run with just `--channel-toml` — the script ships with defaults covering all major platforms, and `--output` defaults to `toolchains/rust/BUCK`:

```bash
uv run python toolchains/rust/gen_rust_toolchain.py \
  --channel-toml https://static.rust-lang.org/dist/2026-07-11/channel-rust-nightly.toml
```

## Defaults

| Category   | Values                                                                         |
|------------|--------------------------------------------------------------------------------|
| **Hosts**  | `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc` |
| **Targets**| `wasm32-unknown-unknown`, `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-gnu`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc` |

## Updating the toolchain

When a new nightly date arrives (check `https://static.rust-lang.org/dist/channel-rust-nightly.toml` for the date):

```bash
DATE=$(curl -s https://static.rust-lang.org/dist/channel-rust-nightly.toml \
  | head -3 | grep '^date' | sed 's/.*"\(.*\)"/\1/')
uv run python toolchains/rust/gen_rust_toolchain.py \
  --channel-toml "https://static.rust-lang.org/dist/$DATE/channel-rust-nightly.toml"
```

## Custom hosts/targets

Override defaults with `--host` / `--target` (repeatable):

```bash
uv run python toolchains/rust/gen_rust_toolchain.py \
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

## Adding a new triple to `_TRIPLE_TO_PLATFORM`

If you need a triple not in the default mapping, edit the `_TRIPLE_TO_PLATFORM` dict in `gen_rust_toolchain.py`:

```python
_TRIPLE_TO_PLATFORM: dict[str, tuple[str, str | None]] = {
    ...
    "new-triple": ("cpu_name", "os"),   # os=None for platform-less targets like wasm
}
```
