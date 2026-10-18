# `reapid`: cache-only REAPI server for buck2

`reapid` speaks buck2's Remote Execution API (REAPI v2) but never executes:
buck2 runs every action locally and uses the daemon purely as a remote
action cache + content-addressable store. Warm caches survive `buck2 clean`
and daemon restarts; misses execute locally and upload.

One binary covers both roles: a pure cache serving its own directory store,
and a multiplexer fronting any number of other `reapid`s (see below). The
difference is just the `[[backends]]` list.

## Run it

```bash
# 1. Build the daemon.
./buck2 build //remote_execution/reapid:reapid

# 2. Write a config (see below) and start it.
./buck2 run //remote_execution/reapid:reapid -- --config /path/to/reapid.toml

# 3. Point buck2 at it via `.buckconfig.local` (gitignored):
```

```ini
[build]
execution_platforms = buckshot//remote_execution/reapid/platform:platform

[buck2_re_client]
action_cache_address = grpc://127.0.0.1:50051
cas_address = grpc://127.0.0.1:50051
engine_address = grpc://127.0.0.1:50051
tls = false
```

`engine_address` points at the daemon too so capability discovery resolves;
nothing ever calls `Execution` because the platform sets
`remote_enabled = False`. Then just `buck2 build //...`: the first build
populates the cache, later builds (even after `buck2 clean`) hit it.

Two buck2 behaviors to know:

- The prelude gates cache *uploads* per action (most rule types default to
  no-upload). Export `BUCK2_TEST_FORCE_CACHE_UPLOAD=1` on populating builds
  or nothing is ever stored (reads need nothing). This is a buck2-side
  default, not a daemon limitation.
- buck2 reads RE config at daemon startup and retries a dead endpoint
  forever. After writing/removing `.buckconfig.local`, `buck2 kill` before
  the next build — and always start the daemon before building against it.

`RUST_LOG=info,reapi_cache=debug` enables per-RPC logging (what buck2 asks
for, which `requestmetadata-bin` fields arrive, eviction decisions). Bare
`RUST_LOG=debug` also turns on the HTTP/2 transport's own firehose.

## Config (`reapid.toml`)

```toml
bind = "127.0.0.1:50051"
store_dir = "/var/cache/reapid"   # created if missing; omit for a pure proxy
digest_functions = ["sha256", "sha1", "blake3"]  # advertised subset
max_store_bytes = 10737418240     # total quota (canonical + zstd bytes)
max_batch_bytes = 4194304         # largest BatchUpdateBlobs payload
max_blob_bytes = 1073741824       # largest single blob on any path

[[backends]]
name = "office"
endpoint = "http://10.0.0.5:50051"

[[backends]]
name = "local"
endpoint = "self"                 # this daemon's own store; always reachable

[[rules]]
name = "keep rust artifacts"
mnemonic = ["Rustc*"]   # globs (`*`, `?`) over the action mnemonic
target = ["//..."]      # globs over the producing target id
pin = true              # never auto-evict

[[rules]]
name = "short-lived tests"
mnemonic = ["*Test*"]
ttl_secs = 86400        # expire past this age even under quota
```

Rules match top-down, first match wins; entries matching nothing (or a rule
with neither `pin` nor `ttl_secs`) live in the default LRU pool. An
eviction pass runs after every mutating RPC and once at startup: TTL-expired
entries go first, then least-recently-used unpinned entries until the store
fits the quota. `min_size_bytes` / `max_size_bytes` and `invocation` (tool
invocation id globs) are further matchers; see `src/config.rs`.

Identifying metadata (mnemonic, target, invocation/config ids) comes from
the `requestmetadata-bin` gRPC header the client attaches to REAPI calls,
plus per-blob size and times. CAS blobs are shared across actions, so
metadata is last-writer-wins. Measured against buck2 2026-09-30: buck2
sends only the tool invocation id (one UUID per build) — `mnemonic` and
`target` matchers stay inert for buck2 traffic (they work for RE clients
that populate them, e.g. Bazel) and size/age/invocation carry policy.
Attributing blobs to producer rules from Action/Command contents instead
of headers is the natural follow-up if rule-aware eviction matters.

## Multiplex mode

`[[backends]]` is required and non-empty; list entries win in order. Each
entry is another `reapid`'s URL or `endpoint = "self"` (this daemon's own
store, which never goes down). A pure cache is `backends = [self]` with a
`store_dir`; a pure proxy lists only remotes and omits `store_dir`
entirely. Config validation keeps the two consistent: `self` without a
store is rejected, as are a store or `[[rules]]` with no `self` to use
them.

Routing, per RPC:

- The build's `tool_invocation_id` (one UUID per buck2 build) pins to the
  first backend whose circuit breaker admits traffic. **Pins never move
  mid-build.** If the pinned backend dies, RPCs fail with `UNAVAILABLE`
  naming the backend instead of silently migrating: buck2's
  [restarter](https://buck2.build/docs/users/advanced/restarter/) brings
  the build back under a fresh invocation id, which routes to a healthy
  backend. Artifacts missing on the new backend surface as ordinary cache
  misses, which the restarter also covers.
- New builds always re-select, so failover and recovery need no operator
  action. Calls without an invocation id take the best backend each time
  (never pinned).
- Health is a per-backend circuit breaker: `max_consecutive_failures`
  (default 3) connectivity failures open it — transport errors plus
  `UNAVAILABLE`/`DEADLINE_EXCEEDED`; application answers, even errors,
  prove liveness and reset it. After `cooldown_secs` (default 30) one
  trial build goes through; success closes the breaker, failure restarts
  the cooldown. If every remote is cooling, the preferred one is attempted
  anyway rather than failing fast.
- List `self` last and offline builds keep working against the local
  cache whenever no remote is reachable.
- Request metadata (`requestmetadata-bin`) is forwarded verbatim, so
  backends still see invocation/mnemonic/target hints for their own
  eviction policy.
- `GetCapabilities` is always answered locally from the mux's own config
  (one stable answer no matter what is up). Keep the mux's advertised
  limits at or under every backend's.

## Store layout

Plain files under `store_dir` (safe to `rm -rf` when the daemon is down):

```text
cas/<sha256|sha1|blake3>/<aa>/<hash>       canonical bytes (verified at ingest)
cas/.../<hash>.zst                         stored zstd form (as-received, or
                                           encoded on demand for a compressed read)
cas/.../<hash>.meta.json                   sizes, times, captured hints
ac/...                                     ActionResults, same sidecar scheme
uploads/                                   partial ByteStream writes (wiped on open)
```

## Protocol notes

- Served: `ContentAddressableStorage`, `ActionCache`, `Capabilities`,
  `ByteStream`. Not served: `Execution` (nothing executes here) and the
  chunking RPCs (`SplitBlob`/`SpliceBlob`/mappings return `UNIMPLEMENTED`;
  clients fall back to whole blobs).
- Digests: SHA-256, SHA-1, BLAKE3, verified before anything is stored.
  Clients that omit `digest_function` get hash-length inference with
  trial verification for the ambiguous 64-hex-char case.
- zstd: accepted on upload (verified post-decode), served passthrough when
  the sender's bytes are on hand, encoded on demand otherwise, and
  advertised in `Capabilities`. No other compressors.
- The empty blob synthesizes as present for every function without storage.
- `GetTree` answers in one response (no paging); trees past 100k
  directories fail rather than truncate.
- `GetActionResult`'s `inline_*` flags are ignored: the stored result is
  returned verbatim and clients fetch output blobs themselves.
- Any `instance_name` is accepted into one global namespace.

## Tests

- `buck2 test //remote_execution/reapid:reapi-cache-unittest` — digest vectors, store,
  policy, config, mux routing/breaker, resource-name and metadata-header units.
- `buck2 test //remote_execution/reapid/tests:roundtrip` — full gRPC round trips against
  an in-process server (batch + streaming + compressed + resume + TTL/LRU
  eviction + sidecar hints). Runs in CI.
- `buck2 test //remote_execution/reapid/tests:mux` — one `reapid` fronting others:
  priority routing, per-build pins, breaker failover, `self` fallback,
  metadata forwarding. Runs in CI.
- `buck2 test //remote_execution/reapid/tests:restart` — populate over gRPC,
  power-cycle the daemon, re-verify everything acknowledged (plus unfinished
  uploads reading as missing and LRU order surviving). Runs in CI.
- `remote_execution/reapid/e2e.sh` — real buck2 build against the real daemon with a
  cache-hit proof (see below). Manual (needs ports + a buck2 daemon).

## E2E proof design

`e2e.sh` builds the fixture at `//remote_execution/reapid/e2e:marker`, a genrule
whose command appends to a marker file *outside* buck-out (an undeclared
side effect buck2 cannot see) while writing its declared output normally.
Build once (marker: 1 line), `kill -9` the daemon and restart it on the
same store (a power-loss drill), `buck2 clean`, build again: if the marker
is still 1 line, the second build was a remote-cache hit served from the
reopened store (the command never re-ran). Any higher count is a cache
miss/regression.
