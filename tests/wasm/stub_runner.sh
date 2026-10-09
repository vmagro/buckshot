#!/usr/bin/env bash
# Stub wasm-bindgen runner for the `wasm_bindgen_pkg` test: records its
# argv and writes canned outputs, so the test verifies the rule's own
# logic (stem normalization, arg passing) without real wasm-bindgen.
set -euo pipefail
out=""; input=""; name=""; version=""
for arg in "$@"; do
    case "$arg" in
        --out=*) out="${arg#--out=}" ;;
        --input=*) input="${arg#--input=}" ;;
        --pkg-name=*) name="${arg#--pkg-name=}" ;;
        --pkg-version=*) version="${arg#--pkg-version=}" ;;
    esac
done
mkdir -p "$out"
printf '%s\n' "$@" > "$out/argv.txt"
cp "$input" "$out/stub_bg.wasm"
printf '{"name": "%s", "version": "%s"}\n' "$name" "$version" > "$out/package.json"
