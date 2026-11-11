# buckshot CI actions

Drop-in GitHub Actions for change-gated buck2 test runs: run `buck2 test`
against the targets affected by a push or PR, optionally combined with
target patterns that always run. Consumers need a buckshot cell (for the
`scripts/affected_targets.bxl` query and the CLI) plus the `./buck2`
dotslash wrapper at the repo root.

> These actions run the buckshot CLI. The `ci changed-targets` command is
> new: use a release that includes it (newer than `cli-v0.1.1`) and refresh
> the vendored manifest with `buckshot update <tag>` so the default
> `buckshot//tools/buckshot:buckshot` target serves it.

## `setup`

Installs dotslash and shares the `buck-out/` cache across runs (keyed by
OS + `./buck2` contents, so buck2/prelude upgrades invalidate it).

```yaml
- uses: vmagro/buckshot/actions/setup@vX.Y.Z
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
