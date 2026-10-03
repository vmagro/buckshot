# third-party/npm

Home of `BUCK`, generated from `package-lock.json` (lockfileVersion 3) — the npm equivalent of `reindeer buckify` for Rust crates. The generator itself is the `node npm buckify` subcommand of `buckshot`, this repo's own maintenance CLI (a `rust_binary` at the repo root — see `buckshot/README.md`), not colocated in this directory, since a buck2-built generator can't safely share a `BUCK` file with the one it overwrites.

`package.json` + `package-lock.json` in this directory are checked into git — they're the single source of truth for which third-party JS packages the repo depends on.

## Adding a package

```bash
cd third-party/npm
# edit package.json's dependencies, then:
npm install --package-lock-only --no-audit --no-fund
cd ../..
buck2 run //buckshot -- node npm buckify
```

## Quick start (regenerating with no changes)

```bash
buck2 run //buckshot -- node npm buckify
```

Pass `--lockfile`/`--out-dir` to point at a different lockfile (both default to the paths in this directory), e.g. `buck2 run //buckshot -- node npm buckify -- --lockfile tests/npm/package-lock.json`.

Every resolved third-party package gets a single `npm_archive` macro call (see `defs.bzl`) — pinned by the tarball's sha256 + byte size, which the generator measures by downloading each tarball once (the registry API reports neither, and buck2's `http_archive` accepts only sha1/sha256, so npm's sha512 integrity hashes can't be used directly; the download is verified against the registry's own `dist.shasum`) — keyed by its exact `package-lock.json` path, and nothing else. There's no aggregate target that pulls in the whole third-party set (reindeer doesn't emit one either); each consumer builds its own `node_modules_tree` (see `node/node_modules_tree.bzl`) naming only the packages it actually needs, e.g.:

```python
load("@buckshot//node:node_modules_tree.bzl", "node_modules_tree")

node_modules_tree(
    name = "node_modules",
    packages = {
        "debug": "//third-party/npm:debug",
        "ms": "//third-party/npm:ms",
    },
)
```

See `tests/npm/hermetic` and `tests/npm/transitive_deps` for full examples.

Re-run whenever the lockfile changes; the output is deterministic for a given input. Fetches go through `reqwest`/`tokio`: the per-version metadata API for each package's expected `dist.shasum`, plus one download of each tarball itself to measure its sha256 + size (verified against that shasum) -- both are required or buck2 re-downloads every tarball after each daemon restart instead of reusing the files on disk. `bin` comes from the lockfile entry directly (npm already embeds it there) and `strip_prefix` is left to `npm_archive`'s own `"package"` default.

## What gets generated

```python
npm_archive(
    name = "picocolors",
    url = "https://registry.npmjs.org/picocolors/-/picocolors-1.1.1.tgz",
    sha256 = "...",
    size_bytes = ...,
)
```

`npm_archive` (in `defs.bzl`) is a macro, not a rule directly — it creates the `http_archive` fetch internally and wires it into the underlying `_npm_archive` rule, so each package only needs the one call above instead of a separate `http_archive` + `npm_archive` pair. It also defaults `package_name` to `name` and `strip_prefix` to `"package"` (the `npm pack` convention) and always passes `visibility = ["PUBLIC"]`, so the generator only ever emits `package_name`/`bin`/`visibility` when a package actually diverges from those defaults -- e.g. a scoped package like `@babel/core` (where `package_name` differs from the buck-safe `name`) or one with a `package.json#bin`. The generator never inspects the tarball, so it never overrides `strip_prefix` -- a package whose tarball's top-level directory isn't `"package"` (rare; `@types/node`'s is one known example) will fail to unpack at build time, and the generator has no way to detect or fix that automatically.

## How package identity works

The lockfile's `packages` map already encodes npm's fully-resolved hoisting decisions as directory paths (e.g. `node_modules/escodegen/node_modules/estraverse` for a nested override). This tool doesn't reimplement any of that resolution — it walks every entry that resolves to a real registry tarball and keys the generated target by its exact lockfile path (leading `node_modules/` stripped once; nested overrides keep their embedded `node_modules/...` segments). `node_modules_tree` (in `node/node_modules_tree.bzl`) reconstructs the same directory structure at build time via hard links, so Node's own runtime resolver does the walking-up-the-tree that npm's resolver already decided on — no dependency-resolution logic needs reimplementing.

Workspace-nested entries (e.g. `apps/foo/node_modules/@scope/bar`, from npm declining to hoist a package that's both a direct dep and a peerDependency) get promoted to the flat root, since this tool always produces one flat tree.

## Generator internals

See `buckshot/README.md` for the CLI as a whole; the npm-specific pieces live in `buckshot/src/npm/`:

- `mod.rs` — CLI args + orchestration for the `node npm buckify` subcommand
- `lockfile.rs` — parses `package-lock.json`, resolves npm's hoisting-path encoding into one `ResolvedPackage` per real registry tarball (`bin` comes straight from the lockfile entry)
- `registry.rs` — downloads each package's tarball to measure its sha256 + size, verifying the download against `registry.npmjs.org`'s per-version `dist.shasum`
- `starlark.rs` — renders the resolved packages as `BUCK` using `serde_starlark`
