# node/toolchain

Hermetic node toolchain backed by official nodejs.org release archives: this directory holds the rule definitions (`node_toolchain.bzl`). The toolchain instances themselves are versioned — one generated `BUCK` per release, each alone in its own directory:

- `node/toolchains/<version>/BUCK` — e.g. `node/toolchains/v26.5.0`

with rolling aliases in the generated `node/toolchains/BUCK` (one `v<major>` alias per major line, each pointing at the latest release). `toolchains//:node` (the default) resolves through the default alias in `node/toolchain/BUCK`, which points at one of the rolling aliases.

The generator itself lives in `buckshot/` at the repo root (the `node toolchain` subcommand of buckshot's own CLI — see `buckshot/README.md`).

## Quick start

```bash
buck2 run //buckshot -- node toolchain --version v26.5.0
# writes node/toolchains/v26.5.0/BUCK + refreshes node/toolchains/BUCK
```

`--host` (repeatable) overrides the default host triples (`aarch64-apple-darwin`, `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`).

Then format the generated files (CI enforces `starlark_fmt` cleanliness):

```bash
./tools/buck/starlark_fmt --config tools/buck/starlark_fmt.json fmt \
  node/toolchains/v26.5.0/BUCK node/toolchains/BUCK
```

Adding a release moves the rolling major alias to it when it is the latest on that line; previous releases stay put, so rolling back is a re-run away (re-running the generator for any remaining release refreshes the aliases from whatever is on disk). Never pass `--output node/toolchain/BUCK` (the generator refuses): that path is reserved for the hand-written rule wiring shared by every release.

## What gets generated

Each versioned `node/toolchains/<version>/BUCK` contains `http_archive` targets (one node distribution per host triple) plus a `node_host_bundle` selecting the right archive by execution platform, wired to the rule definitions in `node/toolchain/node_toolchain.bzl`.

`node/toolchains/BUCK` holds the rolling `v<major>` aliases, also generated (deterministically from the releases on disk — same releases, same bytes).
