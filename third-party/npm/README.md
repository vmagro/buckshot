# npm_buckify

Generate `BUCK` from `package-lock.json` (lockfileVersion 3) — the npm equivalent of `reindeer buckify` for Rust crates. Lives alongside the files it reads/writes (`package.json`, `package-lock.json`, `BUCK`), the same way reindeer's generator lives under `third-party/rust/`.

`package.json` + `package-lock.json` in this directory are checked into git — they're the single source of truth for which third-party JS packages the repo depends on.

## Adding a package

```bash
cd third-party/npm
# edit package.json's dependencies, then:
npm install --package-lock-only --no-audit --no-fund
cd ../..
python3 third-party/npm/npm_buckify.py
```

## Quick start (regenerating with no changes)

```bash
python3 third-party/npm/npm_buckify.py
```

Pass `--lockfile`/`--out-dir` to point at a different lockfile (both default to the paths in this directory).

Every resolved third-party package gets its own `http_archive` (pinned by sha256, since buck2's `http_archive` doesn't accept npm's sha512 integrity hashes) + `npm_archive` pair, keyed by its exact `package-lock.json` path — and nothing else. There's no aggregate target that pulls in the whole third-party set (reindeer doesn't emit one either); each consumer builds its own `node_modules_tree` (see `toolchains/npm.bzl`) naming only the packages it actually needs, e.g.:

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

Re-run whenever the lockfile changes; the output is deterministic for a given input. Downloaded tarballs are cached under `.npm_buckify_cache/` across runs (override with `--cache-dir`).

## What gets generated

```python
http_archive(
    name = "picocolors__archive",
    urls = ["https://registry.npmjs.org/picocolors/-/picocolors-1.1.1.tgz"],
    sha256 = "...",
    type = "tar.gz",
)

npm_archive(
    name = "picocolors",
    archive = ":picocolors__archive",
    package_name = "picocolors",
    strip_prefix = "package",
    visibility = ["PUBLIC"],
)
```

## How package identity works

The lockfile's `packages` map already encodes npm's fully-resolved hoisting decisions as directory paths (e.g. `node_modules/escodegen/node_modules/estraverse` for a nested override). This tool doesn't reimplement any of that resolution — it walks every entry that resolves to a real registry tarball and keys the generated target by its exact lockfile path (leading `node_modules/` stripped once; nested overrides keep their embedded `node_modules/...` segments). `node_modules_tree` (in `toolchains/npm.bzl`) reconstructs the same directory structure at build time via hard links, so Node's own runtime resolver does the walking-up-the-tree that npm's resolver already decided on — no dependency-resolution logic needs reimplementing.

Workspace-nested entries (e.g. `apps/foo/node_modules/@scope/bar`, from npm declining to hoist a package that's both a direct dep and a peerDependency) get promoted to the flat root, since this tool always produces one flat tree.
