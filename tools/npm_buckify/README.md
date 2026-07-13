# npm_buckify

Generate `third-party/npm/BUCK` from an npm `package-lock.json` (lockfileVersion 3) — the npm equivalent of `reindeer buckify` for Rust crates.

## Quick start

```bash
python3 tools/npm_buckify/main.py \
  --lockfile path/to/your/package-lock.json \
  --out-dir third-party/npm
```

Every resolved third-party package gets an `http_archive` (pinned by sha256, since buck2's `http_archive` doesn't accept npm's sha512 integrity hashes) + `npm_archive` pair, keyed by its exact `package-lock.json` path. A `node_modules_tree` target named `node_modules` aggregates all of them into a real `node_modules/` directory — see `toolchains/npm.bzl` for how consumers use it (e.g. `tests/npm/hermetic`).

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

ALL_NPM_PACKAGES = {"picocolors": ":picocolors"}

node_modules_tree(
    name = "node_modules",
    packages = ALL_NPM_PACKAGES,
    visibility = ["PUBLIC"],
)
```

## How package identity works

The lockfile's `packages` map already encodes npm's fully-resolved hoisting decisions as directory paths (e.g. `node_modules/escodegen/node_modules/estraverse` for a nested override). This tool doesn't reimplement any of that resolution — it walks every entry that resolves to a real registry tarball and keys the generated target by its exact lockfile path (leading `node_modules/` stripped once; nested overrides keep their embedded `node_modules/...` segments). `node_modules_tree` (in `toolchains/npm.bzl`) reconstructs the same directory structure at build time via hard links, so Node's own runtime resolver does the walking-up-the-tree that npm's resolver already decided on — no dependency-resolution logic needs reimplementing.

Workspace-nested entries (e.g. `apps/foo/node_modules/@scope/bar`, from npm declining to hoist a package that's both a direct dep and a peerDependency) get promoted to the flat root, since this tool always produces one flat tree.
