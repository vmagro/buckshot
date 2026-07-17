# buckshot

This repo's own maintenance CLI: generators for the `BUCK` files buckshot needs before it can build the things they describe. One `rust_binary`, `clap`-based, with a subcommand per generator, rather than a separate crate/script per generator, so they share one dependency set (vendored in `third-party/rust/`), including a single `#[tokio::main]` runtime and `reqwest` for every fetch.

```bash
buck2 run //buckshot -- <command>
```

## Commands

- `node npm buckify` — generates `third-party/npm/BUCK` from `third-party/npm/package-lock.json`. See `third-party/npm/README.md`.
- `rust toolchain` — generates `rust/toolchain/BUCK` (a `downloaded_rust_toolchain`) from a rustup release-channel TOML. See `rust/toolchain/README.md`.

Run `buck2 run //buckshot -- --help` (or `-- <command> --help`) for the full flag list of either.

## Layout

- `src/main.rs` — `#[tokio::main]` `clap` CLI entry point, dispatches to the two subcommand modules
- `src/node/npm/` — `node npm buckify`: `mod.rs` (CLI args + orchestration, owns the shared `reqwest::Client`), `lockfile.rs` (package-lock.json resolution), `registry.rs` (per-package `dist.shasum` fetch from `registry.npmjs.org`), `starlark.rs` (BUCK rendering)
- `src/rust_toolchain/` — `rust toolchain`: `mod.rs` (CLI args + orchestration), `manifest.rs` (rustup channel TOML parsing + component selection), `starlark.rs` (BUCK rendering)

Both subcommands render their output with `serde_starlark` from strongly-typed structs, rather than hand-formatted strings.

## Why this is a buck2-built binary, and why that's a real bootstrapping risk

Unlike a plain script, this binary's own compilation depends on the `toolchains//:rust` toolchain and the crates in `third-party/rust/` -- both of which already need to exist and parse correctly *before* `buck2 build //buckshot:buckshot` can succeed. That's fine for `node npm buckify` (it only ever writes `third-party/npm/BUCK`, which nothing needed to build `buckshot` itself depends on), but `rust toolchain` writes `rust/toolchain/BUCK` -- the very toolchain `buckshot` needs to compile.

**If a `rust toolchain` run ever produces a broken `BUCK` file, every subsequent `buck2` command fails, including rebuilding `buckshot` to fix it.** The only way out is `git checkout -- rust/toolchain/BUCK` (or restoring a known-good copy some other way) to get a working toolchain back before you can `buck2 build` anything again. Keep `rust/toolchain/BUCK` committed and don't run `rust toolchain` with uncommitted changes elsewhere you're not prepared to `git checkout` around.
