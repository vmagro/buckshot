#!/bin/bash
# PostToolUse hook: format .bzl/BUCK files with starlark_fmt after agent edits.
# stdin: Muse's hook JSON payload (tool_response.filePath, else tool_input.path).
# Exits 0 for non-Starlark files and on success; a formatter failure exits
# non-zero so it shows up in the transcript (it never blocks the session).
set -euo pipefail

input=$(cat)
out=$(printf '%s' "$input" | python3 -c '
import json, os, sys
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(0)
p = (d.get("tool_response") or {}).get("filePath") or (d.get("tool_input") or {}).get("path") or ""
cwd = d.get("cwd") or ""
if p and cwd and not os.path.isabs(p):
    p = os.path.join(cwd, p)
print(p)
print(cwd)
' 2>/dev/null || true)
file=$(printf '%s' "$out" | sed -n '1p')
root=$(printf '%s' "$out" | sed -n '2p')
[ -n "$file" ] || exit 0

case "$file" in
    *.bzl | */BUCK | BUCK) ;;
    *) exit 0 ;;
esac
case "$file" in
    */buck2-prelude/*) exit 0 ;; # vendored upstream, never reformat
esac

[ -n "$root" ] && [ -d "$root" ] && cd "$root" || true
[ -f "$file" ] || exit 0

./buck2 run //tools/buck:starlark_fmt -- --config tools/buck/starlark_fmt.json fmt "$file"
