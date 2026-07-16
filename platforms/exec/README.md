# platforms/exec

A local, container-based NativeLink remote-execution worker that lets Rust builds cross-compile for Linux (both `x86_64` and `aarch64`) from a macOS dev machine, which can't itself produce Linux ELF binaries.

## How it fits together

- `platforms/exec/remote_execution_platform.bzl` — an `ExecutionPlatformInfo` that routes actions to the NativeLink worker instead of running them locally.
- `platforms/exec/execution_platforms.bzl` — combines it with `prelude//platforms:default` (host, tried first) into the single target `.buckconfig`'s `[build] execution_platforms` points at.
- `platforms/exec/BUCK` — wires up `:linux-aarch64-worker` (the one worker, native aarch64) and `:execution_platforms` (the aggregate).
- `platforms/exec/Dockerfile` + `config.json5` — the actual container: a combined NativeLink CAS + scheduler + worker, running natively as aarch64 (no Rosetta tax for the compute-heavy part — see the Dockerfile's comments for why only the `nativelink` binary itself runs under Rosetta).
- `toolchains/rust/rust_dist.bzl`'s `_cross_linker_flags` — the buck2-side half of the actual cross-compilation: when the worker's native triple (`aarch64-unknown-linux-gnu`) differs from the target triple (`x86_64-unknown-linux-gnu`), it sets `RustToolchainInfo.linker_flags` to `--target=x86_64-unknown-linux-gnu`. The prelude's rust build always links through the cxx toolchain's linker (clang here, per the Dockerfile), and this is the supported hook for extra flags to that linker invocation; clang cross-compiles given just `--target=`, using the cross libc/binutils the Dockerfile's `crossbuild-essential-amd64` package provides.
- `platforms/BUCK` — the target platforms (`linux-x86_64`, `linux-aarch64`, ...) you actually build for with `--target-platforms`.

## Build and run the worker

```bash
container build -t buckshot-nativelink -f platforms/exec/Dockerfile .
container run -d --name buckshot-nativelink --rosetta -c 8 -m 8g \
  -p 50051:50051 -p 50061:50061 buckshot-nativelink
```

`--rosetta` only affects the `nativelink` process itself (NativeLink publishes no arm64 image) — every action it dispatches (gcc, rustc, ar, ...) still runs as a genuine native aarch64 process. See the Dockerfile's top comment for the full explanation.

Once running, `.buckconfig`'s `[buck2_re_client]` section (pointing at `grpc://localhost:50051`) plus `[build] execution_platforms` are enough for buck2 to route linux-targeted builds there automatically — see `tests/rust/BUCK`'s `exec_compatible_with` for the piece that actually triggers the routing (a target has to opt in; see below).

## Making a target route here

Any target that should build for Linux needs `exec_compatible_with` set so buck2 skips `prelude//platforms:default` (which can't produce Linux ELF) and falls through to the worker:

```python
rust_binary(
    name = "hello",
    srcs = ["main.rs"],
    exec_compatible_with = select({
        "prelude//os:linux": ["prelude//os/constraints:linux"],
        "DEFAULT": [],
    }),
)
```

This is required per-target (not just on the toolchain) — see [buck2's execution platform resolution docs](https://buck2.build/docs/rule_authors/configurations/#execution-platform-resolution): toolchain-level `target_compatible_with` doesn't participate in execution platform selection, only a target's own `exec_compatible_with` does.

## Verifying it works

```bash
buck2 build tests//rust:hello --target-platforms buckshot//platforms:linux-x86_64
```

Watch for `remote:` (not `local:`) in the build's command counts, and check `file buck-out/.../hello` reports an `x86_64` ELF binary, not `arm64`/Mach-O.

## Stopping/removing the worker

```bash
container stop buckshot-nativelink
container rm buckshot-nativelink
```
