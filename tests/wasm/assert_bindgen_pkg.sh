#!/usr/bin/env bash
# Asserts the `wasm_bindgen_pkg` fixture assembled correctly: the runner
# saw the stem-normalized input plus package flags, and the package
# outputs landed in the package dir.
set -euo pipefail
pkg="$1"
grep -q -- '--input=.*/guest_pkg\.wasm$' "$pkg/argv.txt"
grep -q -- '--pkg-name=guest-pkg$' "$pkg/argv.txt"
grep -q -- '--pkg-version=0\.2\.0$' "$pkg/argv.txt"
grep -q -- '--out=.*' "$pkg/argv.txt"
test -f "$pkg/stub_bg.wasm"
grep -q '"name": "guest-pkg"' "$pkg/package.json"
