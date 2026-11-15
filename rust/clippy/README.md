# buckshot rust clippy

Local runs go through the CLI, CI through the action below — both wrap
the same [`rust_clippy.bxl`](./rust_clippy.bxl) query.

```sh
# check some targets, errors print inline and fail the command
buckshot rust clippy tests//rust/... buckshot//buckshot:buckshot
# check whatever a change touched
buckshot rust clippy $(buckshot ci changed-targets)
# apply machine-applicable suggestions, then re-check leftovers
buckshot rust clippy --fix tests//rust/...
```

`--fix` only applies suggestions marked `MachineApplicable`, skips
suggestions inside macro expansions and overlapping ones, then rebuilds
and reports whatever remains. `--buck2` and `--bxl` override the
executable and query (mirroring the action inputs) when the buckshot
cell lives under another name.

## Action

Runs clippy on the rust targets in a list: the BXL reads each rust
target's `RustcExtraOutputsInfo` provider (`clippy.compile_output.diag_json`
— the same artifact the `[clippy.json]` subtarget exposes) and
materializes it, printing one `<label> <path>` line per checked
target; the step then fails when any diagnostic is error-level.
Materializing builds the clippy compiles, so there is no separate
`buck2 build` — one BXL invocation does it all. The compile alone
cannot gate: the prelude runs it infallible (`--failure-filter`) by
design, recording diagnostics instead of failing — so the action
scans the JSON itself. A toolchain `-D warnings` (like buckshot's own
`deny_on_check_lints`) promotes warnings to errors here too.
Non-rust entries carry no such provider and are skipped, so a mixed
change-affected set feeds in directly:

```yaml
- uses: vmagro/buckshot/rust/clippy@vX.Y.Z
  with:
    targets: ${{ steps.changed.outputs.targets }}
```

Inputs: `targets` (labels or patterns, one per line — a YAML block
scalar is the list idiom; empty skips the check and passes), `buck2`,
`bxl` (provider-reading BXL function; override when the buckshot cell
has another name), `inline-comments` (default `'true'`). Targets
incompatible with the default platform are skipped, like
`buck2 build --skip-incompatible-targets`.

Outputs: `targets` (rust target labels that were checked), `count`,
`checked` (`'false'` when no rust targets were given).

Failures land on the exact lines two ways. Every error diagnostic
becomes a `file,line` check annotation, which GitHub renders inline
in PR diffs and on push commit pages alike — no API access needed, so
this covers pushes too. When a PR context is available (a
`pull_request` event, or a push with exactly one associated open PR),
each diagnostic also becomes an inline review comment on the PR head
commit, skipped when an identical comment already exists. Commenting
is best-effort — it needs the `gh` CLI and `pull-requests: write`,
and warns instead of failing when either is missing (forks with
read-only tokens, lines outside the diff) — and `inline-comments:
'false'` turns it off.

Change-gated clippy, replacing an unscoped `uquery kind(...)` over the
whole repo:

```yaml
- id: changed
  uses: vmagro/buckshot/actions/changed-targets@vX.Y.Z
  with:
    # Narrow the universe to first-party cells when some cell cannot
    # be configured (e.g. one with no platform binding).
    extra-args: --universe root//...
- uses: vmagro/buckshot/rust/clippy@vX.Y.Z
  with:
    targets: ${{ steps.changed.outputs.targets }}
```

Scope the universe to first-party cells: vendored third-party crates
are rust targets too, and clippying them is slow and out of your
control.

## Layout

Everything this action needs lives here:

- [`action.yml`](./action.yml) — the composite action (`resolve` the
  diagnostics via BXL, `check` them with the script below).
- [`rust_clippy.bxl`](./rust_clippy.bxl) — the BXL function (default
  `bxl:` input): keeps the rust targets in a mixed list and prints one
  `<label> <diagnostics path>` line per checked target.
- [`clippy_report.py`](./clippy_report.py) — scans the materialized
  JSON for error-level diagnostics, fails the step, emits `file,line`
  annotations, and posts inline PR review comments when it can.
