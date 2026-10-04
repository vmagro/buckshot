# buckshot

This repo's own maintenance CLI: generators for the `BUCK` files buckshot needs before it can build the things they describe. One `rust_binary`, `clap`-based, with a subcommand per generator, rather than a separate crate/script per generator, so they share one dependency set (vendored in `third-party/rust/`), including a single `#[tokio::main]` runtime and `reqwest` for every fetch.

```bash
buck2 run //buckshot -- <command>
```

## Commands

- `buck update` — refreshes the vendored dotslash manifests (`buck2`, `tools/buck/starlark_fmt`, `tools/buck/rust-project`) from a facebook/buck2 release tag. Pass `--root` to stage into another directory.
- `node npm buckify` — generates `third-party/npm/BUCK` from `third-party/npm/package-lock.json`. See `third-party/npm/README.md`.
- `rust toolchain` — generates a versioned `rust/toolchains/<channel>/<version>/BUCK` (a `downloaded_rust_toolchain`) from a rustup release-channel TOML, then refreshes the rolling aliases in `rust/toolchains/BUCK`. See `rust/toolchain/README.md`.
- `python toolchain` — generates a versioned `python/toolchains/<version>/BUCK` (an `astral_python`) from a `python-build-standalone` release tag, then refreshes the rolling aliases in `python/toolchains/BUCK`. See `python/toolchain/README.md`.
- `node toolchain` — generates a versioned `node/toolchains/<version>/BUCK` (a `downloaded_node_toolchain`) from a nodejs.org release, then refreshes the rolling aliases in `node/toolchains/BUCK`. See `node/toolchain/README.md`.
- `cxx toolchain` — generates a versioned `cxx/toolchains/<version>/BUCK` (a `zig_cxx_toolchain`) from the Zig download index, then refreshes the rolling aliases in `cxx/toolchains/BUCK`. See `cxx/toolchain/README.md`.

Run `buck2 run //buckshot -- --help` (or `-- <command> --help`) for the full flag list of either.

## Layout

- `src/main.rs` — `#[tokio::main]` `clap` CLI entry point, dispatches to the subcommand modules
- `src/buck/` — `buck update`: release resolution + dotslash fetch/validate/write (no Starlark rendering)
- `src/node/npm/` — `node npm buckify`: `mod.rs` (CLI args + orchestration, owns the shared `reqwest::Client`), `lockfile.rs` (package-lock.json resolution), `registry.rs` (per-package `dist.shasum` fetch from `registry.npmjs.org`), `starlark.rs` (BUCK rendering)
- `src/node/toolchain/` — `node toolchain`: `mod.rs` (CLI args + orchestration), `manifest.rs` (`SHASUMS256.txt` parsing + component selection), `starlark.rs` (BUCK rendering)
- `src/rust_toolchain/` — `rust toolchain`: `mod.rs` (CLI args + orchestration), `manifest.rs` (rustup channel TOML parsing + component selection), `starlark.rs` (BUCK rendering)
- `src/python/toolchain/` — `python toolchain`: `mod.rs` (CLI args + orchestration), `manifest.rs` (GitHub release assets + component selection), `starlark.rs` (BUCK rendering)
- `src/cxx_toolchain/` — `cxx toolchain`: `mod.rs` (CLI args + orchestration), `manifest.rs` (Zig download index parsing + component selection), `starlark.rs` (BUCK rendering)
- `src/releases.rs` — shared versioned-release scanning (`<lang>/toolchains/` directories) plus version-identifier parsers for the alias renderers

Every generator renders its output with `serde_starlark` from strongly-typed structs, rather than hand-formatted strings. Format the generated files with `./tools/buck/starlark_fmt --config tools/buck/starlark_fmt.json fmt <files>` before committing (CI enforces it).

Unit tests for the generator logic (channel detection, alias rendering) live in `#[cfg(test)]` modules next to the code and run as `:buckshot-unittest` (see `BUCK`, in CI).

## Why this is a buck2-built binary, and why that's a real bootstrapping risk

Unlike a plain script, this binary's own compilation depends on the `toolchains//:rust` toolchain and the crates in `third-party/rust/` -- both of which already need to exist and parse correctly *before* `buck2 build //buckshot:buckshot` can succeed. That's fine for `node npm buckify` (it only ever writes `third-party/npm/BUCK`, which nothing needed to build `buckshot` itself depends on) and for `buck update` (it only rewrites dotslash manifests, which take effect on the next buck2 invocation and need no build to revert), but `rust toolchain` writes versioned releases under `rust/toolchains/` plus the rolling `rust/toolchains/BUCK` the default `toolchains//:rust` resolves through -- the very toolchain `buckshot` needs to compile. `cxx toolchain` is nearly as risky: it writes `cxx/toolchains/`, which rustc links through, so a broken cxx BUCK also breaks every `buckshot` rebuild.

**If a `rust toolchain` (or `cxx toolchain`) run ever produces a broken `BUCK` file on the default's resolution path, every subsequent `buck2` command fails, including rebuilding `buckshot` to fix it.** Generating a *new* release is the safe case (a new directory can't break the previous one); the risk is a broken alias file or an in-place regeneration. The way out is `git checkout -- rust/toolchains/` (or `cxx/toolchains/`, or restoring a known-good copy some other way) to get a working toolchain back before you can `buck2 build` anything again. Keep those `BUCK` files committed and don't run the generators with uncommitted changes elsewhere you're not prepared to `git checkout` around. (The generators refuse `--output <lang>/toolchain/BUCK` outright: those static files hold the rule wiring shared by every release.)
