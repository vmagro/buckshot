# Buck2 Helpers

A collection of Buck2 helpers: buck2-prelude, toolchains, and utility scripts.

## Structure

- `buck2-prelude/` — Facebook's buck2 prelude pulled as a git subtree
- `third-party/BUCK` — External dependencies via http_archive
- `toolchains/BUCK` + `toolchains/*.bzl` — Toolchain definitions (e.g. rust_toolchain)
- `tests/` — Test suites and test utilities

## Toolchain Generation

To generate a `rust_toolchain` from a rustup dist channel TOML:

```bash
python3 tools/gen_rust_toolchain/main.py <path-to-channel-rust-nightly.toml> <output-directory>
```

This creates:
- `toolchains/rust_dist.bzl` — `host_bundle`, `rust_lld`, and `downloaded_rust_toolchain` rule definitions
- `toolchains/rust_dist/BUCK` — http_archives for all components + toolchain wiring

See `vmcode/build/toolchains/rust_dist/` for an example of what the generated targets look like.
