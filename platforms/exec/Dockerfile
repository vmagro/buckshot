# A local buck2 remote-execution worker for platforms:linux-x86_64 and
# platforms:linux-arm64. Runs *natively* as whatever arch `docker build`
# is invoked on (no --platform needed -- plain `docker build`/`docker run`,
# no Apple-specific tooling), and cross-compiles to the *other* Linux arch
# via a Debian crossbuild-essential package, so there's no emulation tax
# for the compute-heavy part of the build either way: clang/ar/rustc all
# run as native processes, only ever producing foreign-arch output, never
# executing it. See platforms/exec/BUCK (the local-linux-worker execution
# platform, whose CPU tracks whatever machine buck2 itself is invoked on)
# and toolchains/rust/rust_dist.bzl's `_cross_linker_flags` (the buck2-side
# half of this -- adds a `--target=<triple>` linker flag only when
# genuinely cross-compiling).
#
# Build (from the repo root; native arch of the build host, no flags
# needed -- BuildKit populates $TARGETARCH from that automatically):
#   docker build -t buckshot-nativelink -f platforms/exec/Dockerfile .
# Run:
#   docker run -d --name buckshot-nativelink --cpus 8 -m 8g \
#     -p 50051:50051 -p 50061:50061 buckshot-nativelink
#
# Works on any host OS (macOS, Linux, Windows) since it's plain Docker --
# the only requirement is that the *container* is Linux. On an arm64 host,
# running the amd64-only nativelink binary below (see next comment) needs
# the host's Docker to have foreign-arch (binfmt_misc/qemu, or Docker
# Desktop's Rosetta setting) execution support enabled; on an amd64 host
# (e.g. GitHub Actions' ubuntu-latest runners) it just runs natively, no
# emulation needed anywhere.

# NativeLink publishes no arm64 image (amd64-only), and building it from
# source pulls in a whole extra Rust toolchain bootstrap for a binary that
# isn't the performance-sensitive part of this setup anyway -- so grab the
# amd64 binary regardless of the final image's own native arch, and run
# *just that one process* under emulation if needed (see above). Everything
# it actually spawns to build things (gcc, ar, rustc) still runs as genuine
# native processes; only this lightweight orchestrator ever pays an
# emulation cost, and only on non-amd64 hosts.
FROM --platform=linux/amd64 ghcr.io/tracemachina/nativelink:v1.4.0 AS nativelink

FROM debian:bookworm-slim

# clang+lld is the *native* C/C++ toolchain for whatever arch this image
# builds as -- the prelude's default cxx toolchain (buck2-prelude/
# toolchains/cxx.bzl's system_cxx_toolchain, used unmodified here)
# hardcodes the tool names "clang"/"clang++", not whatever happens to be
# `cc`/`c++` on $PATH, and rust build scripts (build.rs binaries) plus
# rust's own linking always go through it. A same-arch *target* build (this
# container's own native arch, no cross needed) links with no extra flags.
#
# crossbuild-essential-<arch> provides the *other* arch's cross binutils +
# libc/crt objects (Debian multiarch layout) -- clang cross-compiles given
# just a `--target=<triple>` flag (unlike gcc, no separate cross binary
# needed), which toolchains/rust/rust_dist.bzl adds whenever the target
# triple differs from this container's own native triple. $TARGETARCH is
# populated automatically by BuildKit from the build's (native, by default)
# target platform -- no --build-arg needed.
ARG TARGETARCH
RUN set -eu; \
    case "$TARGETARCH" in \
        amd64) other_arch=arm64 ;; \
        arm64) other_arch=amd64 ;; \
        *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    dpkg --add-architecture "$other_arch" && \
    apt-get update && \
    apt-get install -y --no-install-recommends \
        clang \
        lld \
        "crossbuild-essential-$other_arch" \
        ca-certificates \
        python3 \
        unzip \
    && rm -rf /var/lib/apt/lists/*

# The upstream image is a nix2container build, not a normal filesystem --
# the binary lives under a content-hashed /nix/store/<hash>-nativelink-.../bin
# path rather than /usr/local/bin, so glob it out by name instead of hardcoding
# the hash (found via `container image inspect`, not documented anywhere).
COPY --from=nativelink /nix/store/*-nativelink-*/bin/nativelink /usr/local/bin/nativelink
COPY platforms/exec/config.json5.tmpl /config/config.json5.tmpl
COPY platforms/exec/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

EXPOSE 50051 50061

# Fills in config.json5.tmpl's CPU placeholder with this container's own
# native arch (buck2's cpu label, not Debian's -- e.g. "x86_64" not "amd64")
# before handing off to nativelink, so the advertised worker capability
# always matches whatever arch this image actually is -- see entrypoint.sh
# and platforms/exec/BUCK's `local-linux-worker` (which derives the same label
# from `host_configuration.cpu` on the buck2 side).
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
