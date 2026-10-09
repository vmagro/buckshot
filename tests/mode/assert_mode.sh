#!/usr/bin/env bash
# Asserts the mode constraint reaches rustc: the plain probe must report
# `debug` (no mode value = constraint default = no extra flags) and the
# release-transitioned probe must report `release`.
set -euo pipefail
"$1" debug
"$2" release
