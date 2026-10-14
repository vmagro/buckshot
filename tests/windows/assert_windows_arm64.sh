#!/usr/bin/env bash
# Asserts each windows-arm64-transitioned binary linked for the right target:
# a `.exe` Windows binary for AArch64. The binaries cannot run on the build
# host, so inspect them instead of executing them.
set -euo pipefail
for bin in "$@"; do
    case "$bin" in
        *.exe) ;;
        *)
            echo "expected a .exe output, got: $bin" >&2
            exit 1
            ;;
    esac
    info=$(file -b "$bin")
    echo "$bin: $info"
    case "$info" in
        *PE32* | *MS\ Windows*) ;;
        *)
            echo "not a Windows PE binary: $info" >&2
            exit 1
            ;;
    esac
    case "$info" in
        *AArch64* | *[Aa]arch64* | *ARM64*) ;;
        *)
            echo "not an ARM64 binary: $info" >&2
            exit 1
            ;;
    esac
done
