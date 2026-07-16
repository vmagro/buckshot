# third-party/npm

Home of `BUCK`, generated from `package-lock.json` (lockfileVersion 3) — the npm equivalent of `reindeer buckify` for Rust crates. The generator itself is the `npm buckify` subcommand of `buckshot`, this repo's own maintenance CLI (a `rust_binary` at the repo root — see `buckshot/README.md`), not colocated in this directory, since a buck2-built generator can't safely share a `BUCK` file with the one it overwrites.

`package.json` + `package-lock.json` in this directory are checked into git — they're the single source of truth for which third-party JS packages the repo depends on.

## Adding a package

```bash
cd third-party/npm
# edit package.json's dependencies, then:
npm install --package-lock-only --no-audit --no-fund
cd ../..
buck2 run //buckshot -- npm buckify
```

## Quick start (regenerating with no changes)

```bash
buck2 run //buckshot -- npm buckify
```

Pass `--lockfile`/`--out-dir` to point at a different lockfile (both default to the paths in this directory), e.g. `buck2 run //buckshot -- npm buckify -- --lockfile tests/npm/package-lock.json`.

Every resolved third-party package gets a single `npm_archive` macro call (see `defs.bzl`) — pinned by sha256, since buck2's `http_archive` doesn't accept npm's sha512 integrity hashes — keyed by its exact `package-lock.json` path, and nothing else. There's no aggregate target that pulls in the whole third-party set (reindeer doesn't emit one either); each consumer builds its own `node_modules_tree` (see `toolchains/npm.bzl`) naming only the packages it actually needs, e.g.:

```python
load("@toolchains//:npm.bzl", "node_modules_tree")

node_modules_tree(
    name = "node_modules",
    packages = {
        "debug": "third-party//npm:debug",
        "ms": "third-party//npm:ms",
    },
)
```

See `tests/npm/hermetic` and `tests/npm/transitive_deps` for full examples.

Re-run whenever the lockfile changes; the output is deterministic for a given input. Downloaded tarballs are cached under `.npm_buckify_cache/` across runs (override with `--cache-dir`). Fetches shell out to `curl` rather than pulling in a Rust HTTP+TLS stack.

## What gets generated

```python
npm_archive(
    name = "picocolors",
    url = "https://registry.npmjs.org/picocolors/-/picocolors-1.1.1.tgz",
    sha256 = "...",
)
```

`npm_archive` (in `defs.bzl`) is a macro, not a rule directly — it creates the `http_archive` fetch internally and wires it into the underlying `_npm_archive` rule, so each package only needs the one call above instead of a separate `http_archive` + `npm_archive` pair. It also defaults `package_name` to `name` and `strip_prefix` to `"package"` (the `npm pack` convention) and always passes `visibility = ["PUBLIC"]`, so the generator only ever emits `package_name`/`strip_prefix`/`bin`/`visibility` when a package actually diverges from those defaults -- e.g. a scoped package like `@babel/core` (where `package_name` differs from the buck-safe `name`), a tarball with no top-level wrapper directory at all (`strip_prefix = None`), or one with a `package.json#bin`.

## How package identity works

The lockfile's `packages` map already encodes npm's fully-resolved hoisting decisions as directory paths (e.g. `node_modules/escodegen/node_modules/estraverse` for a nested override). This tool doesn't reimplement any of that resolution — it walks every entry that resolves to a real registry tarball and keys the generated target by its exact lockfile path (leading `node_modules/` stripped once; nested overrides keep their embedded `node_modules/...` segments). `node_modules_tree` (in `toolchains/npm.bzl`) reconstructs the same directory structure at build time via hard links, so Node's own runtime resolver does the walking-up-the-tree that npm's resolver already decided on — no dependency-resolution logic needs reimplementing.

Workspace-nested entries (e.g. `apps/foo/node_modules/@scope/bar`, from npm declining to hoist a package that's both a direct dep and a peerDependency) get promoted to the flat root, since this tool always produces one flat tree.

## Generator internals

See `buckshot/README.md` for the CLI as a whole; the npm-specific pieces live in `buckshot/src/npm/`:

- `mod.rs` — CLI args + orchestration for the `npm buckify` subcommand
- `lockfile.rs` — parses `package-lock.json`, resolves npm's hoisting-path encoding into one `ResolvedPackage` per real registry tarball
- `tarball.rs` — fetches + sha256-hashes each tarball (cached under `.npm_buckify_cache/`) and inspects it for its top-level `package.json` (`strip_prefix` + `bin`)
- `starlark.rs` — renders the resolved packages as `BUCK` using `serde_starlark`
