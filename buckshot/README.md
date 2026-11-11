# buckshot

This repo's own maintenance CLI: generators for the `BUCK` files buckshot needs before it can build the things they describe. One `rust_binary`, `clap`-based, with a subcommand per generator, rather than a separate crate/script per generator, so they share one dependency set (vendored in `third-party/rust/`), including a single `#[tokio::main]` runtime and `reqwest` for every fetch.

```bash
buck2 run //buckshot -- <command>
```

## Commands

- `buck update` — refreshes the vendored dotslash manifests (`buck2`, `tools/buck/starlark_fmt`, `tools/buck/rust-project`) from a facebook/buck2 release tag. Pass `--root` to stage into another directory.
- `update` — refreshes buckshot's own CLI dotslash manifest (`tools/buckshot/stable`) from a buckshot `v*` release tag (default: newest). Pass `--root` to stage into another directory.
- `node npm buckify` — generates `third-party/npm/BUCK` from `third-party/npm/package-lock.json`. See `third-party/npm/README.md`.
- `rust toolchain` — generates a versioned `rust/toolchains/<channel>/<version>/BUCK` (a `downloaded_rust_toolchain`) from a rustup release-channel TOML, then refreshes the rolling aliases in `rust/toolchains/BUCK`. See `rust/toolchain/README.md`.
- `rust rust-project` — generates rust-analyzer's `rust-project.json` for buck target patterns against the buck-built sysroot (wraps upstream `rust-project`). See `rust/toolchain/README.md`.
- `python toolchain` — generates a versioned `python/toolchains/<version>/BUCK` (an `astral_python`) from a `python-build-standalone` release tag, then refreshes the rolling aliases in `python/toolchains/BUCK`. See `python/toolchain/README.md`.
- `node toolchain` — generates a versioned `node/toolchains/<version>/BUCK` (a `downloaded_node_toolchain`) from a nodejs.org release, then refreshes the rolling aliases in `node/toolchain/README.md`.
- `cxx toolchain` — generates a versioned `cxx/toolchains/<version>/BUCK` (a `zig_cxx_toolchain`) from the Zig download index, then refreshes the rolling aliases in `cxx/toolchains/BUCK`. See `cxx/toolchain/README.md`.
- `ci changed-targets` — lists the buck targets affected by SCM changes since a base revision: the targets owning the changed files (via `git`/`sl` plus `cquery owner` in `scripts/affected_targets.bxl`) and all of their reverse dependencies (`cquery rdeps`). Prints one label per line for `buck2 test` to consume. See `../actions/README.md` for the GitHub Actions that wire this into CI.

Run `buck2 run //buckshot -- --help` (or `-- <command> --help`) for the full flag list of either.

## Change-gated CI (`ci changed-targets`)

```sh
# What would CI run for the current branch vs main?
buck2 run //buckshot -- ci changed-targets --base origin/main

# Just the affected tests:
buck2 run //buckshot -- ci changed-targets --base origin/main --only tests//...

# Feed it straight into the test runner (empty output means nothing to run):
buck2 test $(buck2 run //buckshot -- ci changed-targets --base origin/main)
```

The command resolves `--base` through the merge-base with `--head`
(default `HEAD`) first, so a stale branch compares against its fork point
rather than flagging everything landed since; pass `--no-merge-base` to
compare verbatim, or `--all-files` to treat every tracked file as changed
(what CI uses for a brand-new branch, which has no base commit).
`--universe` scopes the printed targets and defaults to every local cell
(found via `buck2 audit cell`) — note that is broader than `//...`, which
only covers the main cell. `--only` narrows the printed labels to matching
patterns. `--uncommitted` additionally folds the working copy in
(tracked edits plus new files), which is handy locally but left off in CI.
Only target labels go to stdout; the changed files are logged to stderr.
`--scm git|sl|auto` selects the file source (Sapling checkouts want
`--head . --base <bookmark>`); see `--help` for details.

## Layout

- `src/main.rs` — `#[tokio::main]` `clap` CLI entry point, dispatches to the subcommand modules
- `src/ci/` — `ci changed-targets`: SCM file list (git/`sl`) + `buck2 bxl` owner/rdeps query, printing feedable target labels
- `src/buck.rs` — `buck update`: release resolution + dotslash fetch/validate/write (no Starlark rendering)
- `src/update.rs` — `update`: buckshot CLI release resolution + dotslash fetch/validate/write
- `src/dotslash.rs` — shared dotslash-manifest validation + executable bit
- `src/node/npm/` — `node npm buckify`: `mod.rs` (CLI args + orchestration, owns the shared `reqwest::Client`), `lockfile.rs` (package-lock.json resolution), `registry.rs` (per-package `dist.shasum` fetch from `registry.npmjs.org`), `starlark.rs` (BUCK rendering)
- `src/node/toolchain/` — `node toolchain`: `mod.rs` (CLI args + orchestration), `manifest.rs` (`SHASUMS256.txt` parsing + component selection), `starlark.rs` (BUCK rendering)
- `src/rust/` — `rust toolchain` + `rust rust-project`: `mod.rs` (CLI args + orchestration), `manifest.rs` (rustup channel TOML parsing + component selection), `starlark.rs` (BUCK rendering), `rust_project.rs` (rust-project.json generation)
- `src/python/toolchain/` — `python toolchain`: `mod.rs` (CLI args + orchestration), `manifest.rs` (GitHub release assets + component selection), `starlark.rs` (BUCK rendering)
- `src/cxx_toolchain/` — `cxx toolchain`: `mod.rs` (CLI args + orchestration), `manifest.rs` (Zig download index parsing + component selection), `starlark.rs` (BUCK rendering)
- `src/releases.rs` — shared versioned-release scanning (`<lang>/toolchains/` directories) plus version-identifier parsers for the alias renderers

## CLI releases (dotslash)

Consumers use buckshot for its buck rules and `.bzl` macros without
compiling this CLI: its C dependencies don't build from an external
cell, so rules that need the CLI run the prebuilt dotslash binary at
`buckshot//tools/buckshot:buckshot`.

Cutting a release:

1. Tag the commit `vX.Y.Z` and push the tag. The `CLI release`
   workflow runs `buckshot//scripts/build.bxl:build` with
   `--exe buckshot//buckshot:buckshot`, which cross-builds one binary
   per target platform (macos-aarch64, linux-x86_64, linux-aarch64,
   windows-x86_64 and windows-aarch64 via the `windows-gnu` ABI) into
   a single output directory; each file in that directory is uploaded
   as-is as its own `buckshot-*` asset on the GitHub release. The
   `facebook/dotslash-publish-release` action then assembles the
   `buckshot` dotslash manifest from those assets (per
   `.github/workflows/dotslash-config.json`) and attaches it to the
   same release. (No macos-x86_64 binary: Intel Macs get a loud
   dotslash error until one exists.)
2. Run `buck2 run //buckshot:buckshot -- update vX.Y.Z`
   to vendor the published manifest into `tools/buckshot/stable`, then
   commit that.

`tools/buckshot/stable.exe` is a copy of the upstream [DotSlash
Windows Shim](https://github.com/facebook/dotslash/tree/main/windows_shim)
(x86_64), renamed so it resolves and runs the sibling `stable`
manifest on Windows; it is versioned independently of the CLI
releases.

Every generator renders its output with `serde_starlark` from strongly-typed structs, rather than hand-formatted strings. Format the generated files with `./tools/buck/starlark_fmt --config tools/buck/starlark_fmt.json fmt <files>` before committing (CI enforces it).

Unit tests for the generator logic (channel detection, alias rendering) live in `#[cfg(test)]` modules next to the code and run as `:buckshot-unittest` (see `BUCK`, in CI).

## Why this is a buck2-built binary, and why that's a real bootstrapping risk

Unlike a plain script, this binary's own compilation depends on the `toolchains//:rust` toolchain and the crates in `third-party/rust/` -- both of which already need to exist and parse correctly *before* `buck2 build //buckshot:buckshot` can succeed. That's fine for `node npm buckify` (it only ever writes `third-party/npm/BUCK`, which nothing needed to build `buckshot` itself depends on) and for `buck update` and `update` (they only rewrite dotslash manifests, which take effect on the next invocation and need no build to revert), but `rust toolchain` writes versioned releases under `rust/toolchains/` plus the rolling `rust/toolchains/BUCK` the default `toolchains//:rust` resolves through -- the very toolchain `buckshot` needs to compile. `cxx toolchain` is nearly as risky: it writes `cxx/toolchains/`, which rustc links through, so a broken cxx BUCK also breaks every `buckshot` rebuild.

**If a `rust toolchain` (or `cxx toolchain`) run ever produces a broken `BUCK` file on the default's resolution path, every subsequent `buck2` command fails, including rebuilding `buckshot` to fix it.** Generating a *new* release is the safe case (a new directory can't break the previous one); the risk is a broken alias file or an in-place regeneration. The way out is `git checkout -- rust/toolchains/` (or `cxx/toolchains/`, or restoring a known-good copy some other way) to get a working toolchain back before you can `buck2 build` anything again. Keep those `BUCK` files committed and don't run the generators with uncommitted changes elsewhere you're not prepared to `git checkout` around. (The generators refuse `--output <lang>/toolchain/BUCK` outright: those static files hold the rule wiring shared by every release.)
