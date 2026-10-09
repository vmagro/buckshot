#!/usr/bin/env bash
# The custom config's `define` marker must be baked into the bundle --
# the shared config sets no such marker, so this proves the custom
# config drove the build.
set -euo pipefail
grep -rq "custom-config-took-effect" "$1"
