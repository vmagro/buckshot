# buckshot CI actions

Drop-in GitHub Actions for change-gated buck2 runs: list the targets
affected by a push or PR, test them, and clippy the rust ones.
Consumers need a buckshot cell (for the BXL queries and the CLI)
plus the `./buck2` dotslash wrapper at the repo root. The clippy
action's BXL lives with it under `rust/clippy/` instead of here.

> These actions run the buckshot CLI. The `ci changed-targets` command is
> new: use a release that includes it (newer than `cli-v0.1.1`) and refresh
> the vendored manifest with `buckshot update <tag>` so the default
> `buckshot//tools/buckshot:buckshot` target serves it.

## `setup`

Installs dotslash, shares the `buck-out/` cache across runs (keyed by
OS + `./buck2` contents, so buck2/prelude upgrades invalidate it), and
starts a local `reapid` shared-cache daemon in the background via the
pinned buckshot CLI, pointing buck2 at it through `.buckconfig.local`
(actions still execute on the build host while results flow through the
daemon's store). The daemon's store is restored before it starts and
saved afterwards, so it lives across jobs (keyed by OS + `./buck2` +
the pinned manifest). The pinned CLI must be a release that includes
`reapid` — otherwise the step fails; refresh it with
`buckshot update <tag>` (see above).

```yaml
- uses: vmagro/buckshot/actions/setup@vX.Y.Z
```

Inputs: `cache` (restore/save `buck-out/`, default `'true'`),
`buck2-wrapper` (dotslash buck2 path for the cache key, default
`'buck2'`), `reapid` (restore the shared store, start the background
daemon, default `'true'` — set to `'false'` when the job manages its
own daemon lifecycle, e.g. a cache-population flow that starts a daemon
with a custom store), `buckshot` (pinned dotslash manifest used to run
`reapid`, default `'tools/buckshot/stable'`).

## Pruning the shared store before the cache save

The `reapid` store is saved back to the cache in the setup action's
post-step, so without pruning it grows without bound: every run's new
artifacts accumulate next to entries no build touches anymore. End each
job that runs the daemon with a prune step (last step on purpose —
nothing may build after the daemon stops):

```yaml
- name: Prune reapid store to this job's working set
  if: always()
  run: |
    set -euo pipefail
    kill "$(cat "$RUNNER_TEMP/reapid.pid")" 2>/dev/null || true
    for _ in $(seq 1 50); do kill -0 "$(cat "$RUNNER_TEMP/reapid.pid")" 2>/dev/null || break; sleep 0.1; done
    ./tools/buckshot/stable reapid prune --store-dir "$RUNNER_TEMP/reapid-store"
```

The pruner deletes every entry the most recent daemon process never
touched (default `--since` comes from the `.reapid-birth` marker the
daemon writes at startup), while keeping CAS blobs reachable from a
surviving action result. The daemon must be stopped first: the pruner
walks files directly and must not race a live index. See
`remote_execution/reapid/README.md`.

## `changed-targets`

Lists the change-affected targets — the owners of the changed files
plus all of their reverse dependencies (via `cquery owner` +
`cquery rdeps`) — as a newline-separated `targets` output (plus
`count`), for downstream jobs to consume. Change detection is
event-aware — a pull request lists its files, a push its pushed range
(every tracked file for a new branch) — and needs no full history: PRs
list files through the GitHub API (token comes from the runner), pushes
deepen the shallow checkout just past the previous commit, trees only.
Without API access it falls back to a local merge-base, which does
need the history (`fetch-depth: 0`).

```yaml
- id: changed
  uses: vmagro/buckshot/actions/changed-targets@vX.Y.Z
```

Inputs: `if-changed-patterns` (only list affected targets matching
these patterns; empty lists every affected target in the repo — note
that is not the same as `//...`, which only covers the main cell),
`exclude-patterns` (drop affected targets matching these patterns,
e.g. `buckshot//third-party/...`; applies after
`if-changed-patterns`), `buck2`, `buckshot-target` (CLI target;
override when the cell has another name, or point at
`buckshot//buckshot:buckshot` to build from source), `extra-args`
(appended to `buckshot ci changed-targets`).

The detection steps mirror `buck-test`'s own (composite actions cannot
nest sibling actions, so the two stay duplicated by design): `buck-test`
is `changed-targets` plus `buck2 test` for the common case.

## `rust-clippy`

Change-gated clippy lives in [`rust/clippy`](../rust/clippy/) (the
action is a thin wrapper around the `buckshot rust clippy` CLI, which
ships with the BXL there): feed it the `changed-targets` output and it
checks the rust targets in the mix, failing on error-level diagnostics
with exact-line annotations and inline PR review comments. Reference
it as `vmagro/buckshot/rust/clippy@vX.Y.Z` — the default prebuilt CLI
must be a release containing `rust clippy` (newer than `v0.1.3`).

## `rust-docs`

Change-gated docs would rebuild on every touch, so this one lives in
[`rust/docs`](../rust/docs/) and takes explicit query patterns
instead of the `changed-targets` output: it merges every selected
crate's `[doc]` subtarget into one site, always uploads it as a
workflow artifact, and with `pages: 'true'` also stages a Pages
artifact for a downstream `actions/deploy-pages` job. Reference it as
`vmagro/buckshot/rust/docs@vX.Y.Z`:

```yaml
- uses: vmagro/buckshot/rust/docs@vX.Y.Z
  with:
    targets: buckshot//...
    pages: 'true'
```

## `buck-test`

Tests the change-affected targets: the owners of the changed files plus
all of their reverse dependencies (via `cquery owner` + `cquery rdeps`).
Change detection is event-aware — a pull request tests its files, a push
tests its pushed range (every tracked file for a new branch) — and needs
no full history: PRs list files through the GitHub API (token comes from
the runner), pushes deepen the shallow checkout just past the previous
commit, trees only. Without API access it falls back to a local
merge-base, which does need the history (`fetch-depth: 0`). With no test
selection inputs it tests every affected target in the repo.

```yaml
- uses: vmagro/buckshot/actions/buck-test@vX.Y.Z
```

Inputs (pattern lists are one per line — a YAML block scalar is the list
idiom, since action inputs are strings):
- `if-changed-patterns` — only test affected targets matching these
  patterns (e.g. `tests//...`). Leave empty to test every affected target
  in the repo — note that is not the same as `//...`, which only covers
  the main cell.
- `always-patterns` — patterns to test on every run whether affected or
  not (e.g. a lint suite). Leave empty for change-gated tests only.
- `test-args` — extra space-separated arguments for `buck2 test`.
- `buck2` — buck2 executable/wrapper (default `./buck2`).
- `buckshot-target` — CLI target (default the vendored prebuilt
  `buckshot//tools/buckshot:buckshot`; override when the cell has another
  name, or point at `buckshot//buckshot:buckshot` to build from source).
- `extra-args` — appended to `buckshot ci changed-targets`.

Outputs: `targets` (tested labels and patterns), `count`, `tested`
(`'false'` when nothing ran).

Combinations: `always-patterns` alone tests those patterns plus every
affected target; `if-changed-patterns` alone narrows the affected set
(e.g. only run the `tests//...` subset a change touches); both together
test the always patterns plus the narrowed affected set. With neither, the
whole affected set runs — and with nothing affected, the step passes
without invoking `buck2 test` at all.

## Full example

`.github/workflows/buck.yml` in a consumer repo:

```yaml
name: Buck

on:
  push:
    branches: ["main"]
  pull_request:
    branches: ["main"]

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      # Default shallow checkout is fine: PRs list files through the API,
      # pushes deepen just past the previous commit.
      - uses: actions/checkout@v7
      - uses: vmagro/buckshot/actions/setup@vX.Y.Z
      - uses: vmagro/buckshot/actions/buck-test@vX.Y.Z
```

A repo that also always runs a lint suite but only change-gates `tests//...`:

```yaml
      - uses: vmagro/buckshot/actions/buck-test@vX.Y.Z
        with:
          always-patterns: lint//...
          if-changed-patterns: tests//...
```

## Manual runs

Dispatch events carry no change range, so change-gating is meaningless
there — accept a single `patterns` input instead and forward it as
`always-patterns`, from the Run workflow form in the UI, or scripted:

```yaml
on:
  workflow_dispatch:
    inputs:
      patterns:
        description: 'Target patterns to test (space-separated).'
        required: false
        default: ''
        type: string
```

```yaml
      - uses: vmagro/buckshot/actions/buck-test@vX.Y.Z
        with:
          always-patterns: ${{ inputs.patterns }}
```

```sh
gh workflow run buck.yml --ref my-branch \
  --field patterns="tests//rust/..."
```

Push and pull-request events leave `inputs` empty, so the same workflow
file keeps its default behavior there. (The UI field is single-line, so
space-separate multiple patterns for manual runs.)

This repo's own [buck.yml](../.github/workflows/buck.yml) is the living
version of the minimal example (it uses `./actions/...` and builds the
CLI from source, since the vendored prebuilt only gains commands on CLI
releases).
