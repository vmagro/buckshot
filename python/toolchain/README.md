# python/toolchain

Hermetic python toolchain backed by `python-build-standalone` archives: this directory holds the rule definitions (`astral_python.bzl`). The toolchain instances themselves are versioned — one generated `BUCK` per release, each alone in its own directory:

- `python/toolchains/<version>/BUCK` — e.g. `python/toolchains/3.14.6`

with rolling aliases in the generated `python/toolchains/BUCK` (one `<major>.<minor>` alias per minor line, each pointing at the latest patch release). `toolchains//:python` (the default) and `toolchains//:python_bootstrap` both resolve through the default alias in `python/toolchain/BUCK`, which points at one of the rolling aliases.

The generator itself lives in `buckshot/` at the repo root (the `python toolchain` subcommand of buckshot's own CLI — see `buckshot/README.md`).

## Quick start

```bash
buck2 run //buckshot -- python toolchain --python-version 3.14
# writes python/toolchains/<full-version>/BUCK + refreshes python/toolchains/BUCK
```

`--python-version` takes a `<major>.<minor>` (a release bundles many CPython versions; the generator resolves the exact patch). `--tag` pins the `python-build-standalone` release (defaults to GitHub's `latest`). `--host` (repeatable) overrides the default host triples (`aarch64-apple-darwin`, `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`).

Then format the generated files (CI enforces `starlark_fmt` cleanliness):

```bash
./tools/buck/starlark_fmt --config tools/buck/starlark_fmt.json fmt \
  python/toolchains/<version>/BUCK python/toolchains/BUCK
```

Adding a release moves the rolling minor alias to it when it is the latest patch on that line; previous releases stay put, so rolling back is a re-run away (re-running the generator for any remaining release refreshes the aliases from whatever is on disk). Never pass `--output python/toolchain/BUCK` (the generator refuses): that path is reserved for the hand-written rule wiring shared by every release.

## What gets generated

Each versioned `python/toolchains/<version>/BUCK` contains `http_archive` targets (one `install_only` CPython archive per host triple) plus a `python_host_bundle` selecting the right archive by execution platform, wired to the rule definitions in `python/toolchain/astral_python.bzl`.

`python/toolchains/BUCK` holds the rolling `<major>.<minor>` aliases, also generated (deterministically from the releases on disk — same releases, same bytes).
