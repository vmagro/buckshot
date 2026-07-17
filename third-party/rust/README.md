# third-party/rust

Rust third-party crates, buckified via [reindeer](https://github.com/facebookincubator/reindeer). Lives alongside the files it reads/writes (`reindeer.toml`, `Cargo.toml`, `Cargo.lock`, `BUCK`), the same way `third-party/npm/` holds its own npm equivalents.

Like `third-party/npm/`, this directory doesn't vendor crate sources into git — reindeer's default (no `reindeer vendor` step run here) is to buckify straight to sha256-pinned `http_archive` fetches from `static.crates.io`, matching the pinned-tarball approach `buckshot node npm buckify` uses for npm packages.

These are the crates `buckshot/` itself (the repo's `node npm buckify` + `rust toolchain` generator CLI, at the repo root) depends on — see `buckshot/README.md`.

## Adding a dependency

```bash
cd third-party/rust
# edit Cargo.toml's [dependencies], then:
reindeer buckify
cd ../..
```

`reindeer buckify` resolves the dependency graph (updating `Cargo.lock`) and regenerates `BUCK`. It needs a real `cargo`/`rustc` on `PATH` (reindeer drives cargo metadata, it doesn't reimplement resolution) — this step runs outside buck2, the same way `npm install --package-lock-only` does for `third-party/npm/`.

## Build scripts (`fixups/`)

Reindeer refuses to guess whether a crate's `build.rs` needs to run — any crate with one gets a `fixups/<crate>/fixups.toml` saying `buildscript.run = true` or `= false`. Most build scripts here just probe the toolchain to set optional `--cfg`s and are safe to skip (`= false`); a few (`serde`, `serde_core`, `serde_derive`, `serde_json`, `proc-macro2`, `rustix`, `libc`) need `= true` (some also need `cargo_env = ["CARGO_PKG_VERSION_PATCH"]` since that's normally a real `cargo`-set env var, not something rustc has). If `reindeer buckify` warns about a crate with no fixup, check whether the equivalent `frc_rs` third-party fixup already has the answer before guessing.

## `top/`

`Cargo.toml` can't be *all* dependencies — Cargo insists on at least one real target — so `top/main.rs` is a dummy `[[bin]]` that exists only to keep `cargo`/`reindeer` happy. It's never referenced from any `BUCK` file.
