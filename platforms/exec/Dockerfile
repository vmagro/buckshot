# A local buck2 remote-execution worker for platforms:linux-aarch64 and
# platforms:linux-x86_64 -- runs *natively* as aarch64 (build machines here
# are Apple Silicon, and there's no reason to pay Rosetta emulation tax for
# the whole container just because one of the two target arches is x86_64).
# Instead, this image carries a real x86_64 cross toolchain (Debian's
# crossbuild-essential-amd64) and genuinely cross-compiles: clang/ar/rustc
# all run as native aarch64 processes producing x86_64 output, no emulation
# anywhere in the hot path. See platforms/exec/BUCK (the linux-aarch64-worker
# execution platform) and toolchains/rust/rust_dist.bzl's
# `_cross_linker_flags` (the buck2-side half of this -- adds a
# `--target=x86_64-unknown-linux-gnu` linker flag only when genuinely
# cross-compiling).
#
# Build (from the repo root, via Apple's `container` CLI, not docker --
# native arm64, so no --arch/--platform flag needed):
#   container build -t buckshot-nativelink -f platforms/exec/Dockerfile .
# Run (--rosetta is only for the nativelink binary itself, see below --
# everything it spawns to actually build things runs native aarch64):
#   container run -d --name buckshot-nativelink --rosetta -c 8 -m 8g \
#     -p 50051:50051 -p 50061:50061 buckshot-nativelink

# NativeLink publishes no arm64 image (amd64-only), and building it from
# source pulls in a whole extra Rust toolchain bootstrap for a binary that
# isn't the performance-sensitive part of this setup anyway -- so grab the
# amd64 binary and run *just that one process* under Rosetta (`container run
# --rosetta`, transparently executes foreign-arch ELF binaries inside an
# otherwise-native container). Everything it actually spawns to build things
# (gcc, ar, rustc) still runs as genuine native aarch64 processes; only this
# lightweight orchestrator pays the emulation cost.
FROM --platform=linux/amd64 ghcr.io/tracemachina/nativelink:v1.4.0 AS nativelink

FROM debian:bookworm-slim

# clang+lld is the *native* aarch64 C/C++ toolchain -- the prelude's
# default cxx toolchain (buck2-prelude/toolchains/cxx.bzl's
# system_cxx_toolchain, used unmodified here) hardcodes the tool names
# "clang"/"clang++", not whatever happens to be `cc`/`c++` on $PATH, and
# rust build scripts (build.rs binaries) plus rust's own linking always go
# through it. Also lets a linux-aarch64 *target* build (this container's
# own native arch, no cross needed) link with no extra flags.
# crossbuild-essential-amd64 provides x86_64 cross binutils + libc/crt
# objects (Debian multiarch layout) -- clang cross-compiles given just a
# `--target=x86_64-unknown-linux-gnu` flag (unlike gcc, no separate cross
# binary needed), which toolchains/rust/rust_dist.bzl adds for target
# linux-x86_64.
RUN dpkg --add-architecture amd64 && \
    apt-get update && \
    apt-get install -y --no-install-recommends \
        clang \
        lld \
        crossbuild-essential-amd64 \
        ca-certificates \
        python3 \
    && rm -rf /var/lib/apt/lists/*

# The upstream image is a nix2container build, not a normal filesystem --
# the binary lives under a content-hashed /nix/store/<hash>-nativelink-.../bin
# path rather than /usr/local/bin, so glob it out by name instead of hardcoding
# the hash (found via `container image inspect`, not documented anywhere).
COPY --from=nativelink /nix/store/*-nativelink-*/bin/nativelink /usr/local/bin/nativelink
COPY platforms/exec/config.json5 /config/config.json5

EXPOSE 50051 50061

ENTRYPOINT ["/usr/local/bin/nativelink"]
CMD ["/config/config.json5"]
