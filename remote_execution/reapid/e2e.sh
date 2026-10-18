#!/bin/bash
# End-to-end: real buck2 builds against the real daemon, with a cache-hit proof.
#
#   remote_execution/reapid/e2e.sh
#
# Builds `reapid`, starts it on an ephemeral port with a tempdir store,
# layers a `.buckconfig.local` (restored afterwards) wiring buck2's remote
# cache at the daemon, then builds the `//remote_execution/reapid/e2e:marker`
# fixture twice with `buck2 clean` + `buck2 kill` between. Between the builds
# the daemon is `kill -9`ed and restarted on the same store (a power-loss
# drill): the fixture's command appends to /tmp/reapid-e2e-marker (outside
# buck-out, invisible to buck2), so the marker must have exactly 1 line
# after BOTH builds, proving the second build was a remote-cache hit served
# from the reopened store that never re-ran the command.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

MARKER=/tmp/reapid-e2e-marker
WORK=$(mktemp -d /tmp/reapid-e2e.XXXXXX)
CONFIG="$WORK/reapid.toml"
STORE="$WORK/store"
SAVED_LOCAL="$WORK/buckconfig.local.saved"
DAEMON_PID=""

fail() {
  echo "E2E FAILED: $1" >&2
  echo "--- daemon log ---" >&2
  tail -30 "$WORK/daemon.log" >&2 || true
  exit 1
}

cleanup() {
  if [ -n "$DAEMON_PID" ]; then kill "$DAEMON_PID" 2>/dev/null || true; fi
  if [ -f "$SAVED_LOCAL" ]; then
    mv "$SAVED_LOCAL" .buckconfig.local
  else
    rm -f .buckconfig.local
  fi
  # Drop the RE config from the running daemon too, so later plain builds
  # never dial this script's (now dead) port.
  ./buck2 kill >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

[ -f .buckconfig.local ] && cp .buckconfig.local "$SAVED_LOCAL" || true

# Build the daemon with no RE config in force: a stale .buckconfig.local
# pointing at a dead port would hang this build in a retry loop.
rm -f .buckconfig.local
./buck2 kill >/dev/null 2>&1 || true

echo "--- building reapid"
./buck2 build //remote_execution/reapid:reapid --show-output > "$WORK/built.txt" 2>"$WORK/build.log" \
  || { tail -20 "$WORK/build.log" >&2; exit 1; }
BUILT="$ROOT/$(awk '{print $NF}' "$WORK/built.txt" | tail -1)"
[ -x "$BUILT" ] || fail "no daemon binary at $BUILT"
# Copy out of buck-out: `buck2 clean` below would unlink it mid-run.
DAEMON="$WORK/reapid"
cp "$BUILT" "$DAEMON"

PORT=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
cat > "$CONFIG" <<EOF
bind = "127.0.0.1:$PORT"
store_dir = "$STORE"

[[backends]]
name = "local"
endpoint = "self"
EOF

start_daemon() {
  echo "--- starting daemon on 127.0.0.1:$PORT"
  : > "$WORK/daemon.log"
  RUST_LOG=info,reapi_cache=debug "$DAEMON" --config "$CONFIG" > "$WORK/daemon.log" 2>&1 &
  DAEMON_PID=$!
  for _ in $(seq 1 50); do
    grep -q "serving REAPI" "$WORK/daemon.log" 2>/dev/null && break
    sleep 0.1
  done
  grep -q "serving REAPI" "$WORK/daemon.log" || fail "daemon never came up"
}
start_daemon

cat > .buckconfig.local <<EOF
[build]
execution_platforms = buckshot//remote_execution/reapid/platform:platform

[buck2_re_client]
action_cache_address = grpc://127.0.0.1:$PORT
cas_address = grpc://127.0.0.1:$PORT
engine_address = grpc://127.0.0.1:$PORT
tls = false
EOF

rm -f "$MARKER"
echo "--- clean slate"
# buck2 reads RE config at daemon startup: kill AFTER writing .buckconfig.local
# (a stale daemon keeps dialing the old port; a missing daemon sends builds
# into a capabilities-retry loop, so the daemon must already be up here).
./buck2 kill >/dev/null 2>&1 || true
./buck2 clean >/dev/null 2>&1 || true

# The prelude gates cache uploads per action; force them on (build 1 only:
# build 2's hits exercise the pure read path).
echo "--- build 1 (cold: runs the command, populates the cache)"
BUCK2_TEST_FORCE_CACHE_UPLOAD=1 ./buck2 build //remote_execution/reapid/e2e:marker 2>&1 | tail -3
[ "$(cat "$MARKER")" = "ran" ] || fail "build 1 did not run the command"
[ "$(wc -l < "$MARKER" | tr -d ' ')" = "1" ] || fail "build 1 marker has wrong line count"
echo "--- store after build 1:"
find "$STORE" -type f | head -20
[ -n "$(find "$STORE" -type f | head -1)" ] || fail "store is empty after build 1 (no uploads?)"

echo "--- power-loss drill: kill -9 the daemon, restart on the same store"
kill -9 "$DAEMON_PID" 2>/dev/null || true
wait "$DAEMON_PID" 2>/dev/null || true
start_daemon

echo "--- build 2 (after clean+kill+daemon restart: must be a pure cache hit)"
./buck2 clean >/dev/null 2>&1
./buck2 kill >/dev/null 2>&1 || true
./buck2 build //remote_execution/reapid/e2e:marker 2>&1 | tail -3
[ "$(wc -l < "$MARKER" | tr -d ' ')" = "1" ] || fail "cache miss: marker has $(wc -l < "$MARKER") lines, want 1"

echo
echo "E2E OK: second build was a remote-cache hit (command ran once total)"
