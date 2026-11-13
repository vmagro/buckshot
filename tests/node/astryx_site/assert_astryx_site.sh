#!/usr/bin/env bash
# The app's rendered text must survive the full `vite_bundle` pipeline --
# work-tree staging, the `tsc --noEmit` gate, and the Vite build -- into
# the emitted `dist/`.
set -euo pipefail
grep -rq "Hello World" "$1"
