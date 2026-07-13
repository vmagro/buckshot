#!/usr/bin/env python3
"""Generate a `third-party/npm/BUCK` file from an npm `package-lock.json`
(lockfileVersion 3) -- the npm equivalent of what `reindeer buckify` does
for Rust crates.

The lockfile's `packages` map already encodes npm's fully-resolved
hoisting decisions as directory paths (e.g.
`node_modules/escodegen/node_modules/estraverse` for a nested override).
This tool doesn't reimplement any of that resolution -- it just walks
every entry that resolves to a real registry tarball, fetches +
sha256-hashes it (buck2's `http_archive` only accepts sha256/sha1, not
npm's sha512 subresource-integrity hashes), and emits one `http_archive`
+ `npm_archive` pair per entry, keyed by its exact lockfile path so
`node_modules_tree` (see `toolchains/npm.bzl`) can reconstruct the same
directory structure and let Node's own runtime resolver do the
walking-up-the-tree that npm's resolver already decided on.

Each package gets its own `npm_archive` target and nothing else -- no
aggregate `node_modules_tree` pulling in the whole third-party set is
emitted, the same way reindeer doesn't emit one target that vendors
every crate. Consumers build their own `node_modules_tree` (see
`toolchains/npm.bzl`) naming only the packages they actually need, e.g.
`{"debug": "third-party//npm:debug", "ms": "third-party//npm:ms"}`.

`third-party/npm/package.json` + `third-party/npm/package-lock.json` are
checked into git, the same way reindeer's `third-party/rust/Cargo.toml`
+ `Cargo.lock` are -- they're the single source of truth for which
third-party JS packages the repo depends on. Add a package by editing
`third-party/npm/package.json` and running `npm install
--package-lock-only` in that directory to update the lockfile, then
rerun this script to regenerate `third-party/npm/BUCK`.

Usage:

    python3 third-party/npm/npm_buckify.py

Re-run any time the lockfile changes; the generated file is
deterministic for a given input.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import sys
import tarfile
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path


@dataclass
class ResolvedPackage:
    # Lockfile path with the single leading `node_modules/` stripped.
    # Nested overrides keep their embedded `node_modules/...` segments,
    # e.g. `escodegen/node_modules/estraverse`.
    relpath: str
    # Buck target name: `relpath` with every `/`-separated segment
    # (dropping literal `node_modules` segments) joined by `+`.
    target_name: str
    # npm-style package name, e.g. `@babel/core`.
    package_name: str
    url: str
    sha256: str
    # The tarball's actual top-level directory name (usually `package`,
    # per `npm pack` convention, but not universally -- some tarballs
    # (e.g. `@types/*`) use a different name, and some have none at
    # all). `None` means the tarball's entries sit at its root.
    strip_prefix: str | None
    # binname -> path relative to the package dir, from this package's
    # own `package.json#bin`.
    bin: dict[str, str] = field(default_factory=dict)


def path_segments(relpath: str) -> list[str]:
    """Splits a stripped lockfile path into name segments, dropping the
    literal `node_modules` path components that separate nesting levels."""
    return [s for s in relpath.split("/") if s != "node_modules"]


def derive_package_name(relpath: str) -> str:
    segs = path_segments(relpath)
    last = len(segs) - 1
    if last > 0 and segs[last - 1].startswith("@"):
        return f"{segs[last - 1]}/{segs[last]}"
    return segs[last]


def derive_target_name(relpath: str) -> str:
    return "+".join(path_segments(relpath))


def default_tarball_url(name: str, version: str) -> str:
    """npm's standard registry tarball URL layout:
    `https://registry.npmjs.org/{name}/-/{basename}-{version}.tgz`, where
    `{basename}` drops any `@scope/` prefix. Used when a lockfile entry
    has no `resolved` field of its own (npm omits it for entries it
    considers exact duplicates of content resolved elsewhere) but does
    have a name + version."""
    basename = name.rsplit("/", 1)[-1]
    return f"https://registry.npmjs.org/{name}/-/{basename}-{version}.tgz"


def fetch_cached(url: str, cache_dir: Path) -> bytes:
    cache_key = hashlib.sha256(url.encode()).hexdigest()
    cache_path = cache_dir / f"{cache_key}.tgz"
    if cache_path.exists():
        return cache_path.read_bytes()
    req = urllib.request.Request(url, headers={"User-Agent": "npm_buckify"})
    with urllib.request.urlopen(req) as resp:
        data = resp.read()
    cache_path.write_bytes(data)
    return data


@dataclass
class TarballInfo:
    strip_prefix: str | None
    bin: dict[str, str]


def extract_bin(pkg: dict) -> dict[str, str]:
    raw = pkg.get("bin")
    if isinstance(raw, str):
        name = pkg.get("name", "")
        bare = name.rsplit("/", 1)[-1]
        return {bare: raw}
    if isinstance(raw, dict):
        return {k: v for k, v in raw.items() if isinstance(v, str)}
    return {}


def read_tarball_info(tarball: bytes) -> TarballInfo:
    """Finds the tarball's top-level `package.json` (whatever the
    top-level directory is actually named -- `npm pack` conventionally
    uses `package/`, but that's not universal, e.g. `@types/*` tarballs
    use their own directory name) and returns its normalized `bin` field
    (npm allows `bin` to be either a bare string -- shorthand for a
    single binary named after the package -- or an object mapping
    binnames to paths) alongside the discovered directory name to use as
    this package's `http_archive` `strip_prefix`."""
    with tarfile.open(fileobj=io.BytesIO(tarball), mode="r:gz") as archive:
        for member in archive.getmembers():
            comps = [c for c in member.name.split("/") if c]
            if len(comps) == 2 and comps[1] == "package.json":
                strip_prefix, is_top = comps[0], True
            elif len(comps) == 1 and comps[0] == "package.json":
                strip_prefix, is_top = None, True
            else:
                is_top = False
            if not is_top:
                continue
            fobj = archive.extractfile(member)
            if fobj is None:
                continue
            pkg = json.loads(fobj.read().decode())
            return TarballInfo(strip_prefix=strip_prefix, bin=extract_bin(pkg))
    raise ValueError("no top-level package.json entry found in tarball")


def starlark_str(s: str) -> str:
    """Quotes a string as a Starlark string literal. Real npm tarball
    metadata has turned out weirder than expected (e.g. `@types/node`'s
    top-level tarball directory is literally `node v22.19`, spaces and
    all) -- escape defensively rather than assume any field is a safe
    bare identifier."""
    out = ['"']
    for c in s:
        if c == "\\":
            out.append("\\\\")
        elif c == '"':
            out.append('\\"')
        elif c == "\n":
            out.append("\\n")
        else:
            out.append(c)
    out.append('"')
    return "".join(out)


def render_buck_file(pkgs: list[ResolvedPackage], lockfile: str) -> str:
    out = []
    out.append(f"# @generated by third-party/npm/npm_buckify.py from {lockfile}.\n")
    out.append("# Do not edit by hand -- rerun third-party/npm/npm_buckify.py to regenerate.\n\n")
    out.append('load("@prelude//:rules.bzl", "http_archive")\n')
    out.append('load(":defs.bzl", "npm_archive")\n\n')

    for pkg in pkgs:
        out.append(
            f'http_archive(\n    name = {starlark_str(pkg.target_name + "__archive")},\n'
            f"    urls = [{starlark_str(pkg.url)}],\n"
            f"    sha256 = {starlark_str(pkg.sha256)},\n"
            '    type = "tar.gz",\n)\n\n'
        )
        out.append(
            f"npm_archive(\n    name = {starlark_str(pkg.target_name)},\n"
            f'    archive = ":{pkg.target_name}__archive",\n'
            f"    package_name = {starlark_str(pkg.package_name)},\n"
        )
        if pkg.strip_prefix is not None:
            out.append(f"    strip_prefix = {starlark_str(pkg.strip_prefix)},\n")
        if pkg.bin:
            out.append("    bin = {\n")
            for k, v in sorted(pkg.bin.items()):
                out.append(f"        {starlark_str(k)}: {starlark_str(v)},\n")
            out.append("    },\n")
        out.append('    visibility = ["PUBLIC"],\n)\n\n')

    return "".join(out).rstrip() + "\n"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument(
        "--lockfile",
        type=Path,
        default=Path("third-party/npm/package-lock.json"),
        help="Path to package-lock.json (lockfileVersion 3).",
    )
    parser.add_argument("--out-dir", type=Path, default=Path("third-party/npm"), help="Directory to write the generated BUCK file into.")
    parser.add_argument("--cache-dir", type=Path, default=Path(".npm_buckify_cache"), help="Directory to cache downloaded tarballs in across runs.")
    args = parser.parse_args()

    lockfile = json.loads(args.lockfile.read_text())
    packages: dict[str, dict] = lockfile["packages"]

    args.cache_dir.mkdir(parents=True, exist_ok=True)

    # Any entry nested under some `node_modules/` (whether at the
    # workspace root or under a workspace member) is a third-party
    # package by definition -- our own workspace members' own entries
    # never contain `node_modules/` in their key at all. Two shapes
    # show up:
    #
    # - Rooted at `node_modules/...` -- the common case.
    # - Rooted at some workspace member instead, e.g.
    #   `apps/foo/node_modules/@scope/bar` -- npm's own hoisting choice,
    #   not staleness: some packages declare a dependency as both a
    #   direct dep and a peerDependency with the same range, and npm
    #   consistently keeps those un-hoisted, giving every consumer its
    #   own nested copy instead of one shared root copy. Since a flat
    #   `node_modules_tree` only has one root, any such package gets
    #   promoted there. These entries also frequently have no
    #   `resolved`/`integrity` at all -- npm considers them exact
    #   duplicates of content resolved elsewhere and omits the
    #   redundant metadata -- so the tarball URL gets synthesized from
    #   the (always present) name + version using npm's standard
    #   registry layout instead.
    qualifying = [
        (key, entry)
        for key, entry in packages.items()
        if "node_modules/" in key and entry.get("version")
    ]
    # Root-prefixed entries first (always authoritative), then
    # workspace-nested ones (only promoted if not already claimed) -- an
    # explicit two-pass order, not left to string sort coincidentally
    # placing "node_modules/..." before/after "apps/"/"packages/...".
    qualifying.sort(key=lambda kv: (not kv[0].startswith("node_modules/"), kv[0]))

    print(f"npm_buckify: fetching {len(qualifying)} packages...", file=sys.stderr)

    resolved: list[ResolvedPackage] = []
    seen_targets: dict[str, str] = {}
    claimed_names: dict[str, str] = {}

    for key, entry in qualifying:
        is_root = key.startswith("node_modules/")
        if is_root:
            relpath = key[len("node_modules/") :]
        else:
            idx = key.rfind("node_modules/")
            if idx == -1:
                continue
            relpath = key[idx + len("node_modules/") :]

        package_name = derive_package_name(relpath)
        if not is_root:
            prev_key = claimed_names.get(package_name)
            if prev_key is not None:
                print(f"  (skipping {key}, already have {package_name} via {prev_key})", file=sys.stderr)
                continue
        claimed_names[package_name] = key

        target_name = derive_target_name(relpath)
        prev = seen_targets.setdefault(target_name, relpath)
        if prev != relpath:
            raise ValueError(f"target name collision: {relpath!r} and {prev!r} both map to {target_name!r}")

        resolved_url = entry.get("resolved")
        if resolved_url and resolved_url.startswith("http"):
            url = resolved_url
        else:
            url = default_tarball_url(package_name, entry["version"])

        print(f"  [{len(resolved) + 1}/{len(qualifying)}] {relpath}", file=sys.stderr)
        data = fetch_cached(url, args.cache_dir)
        sha256 = hashlib.sha256(data).hexdigest()
        info = read_tarball_info(data)

        resolved.append(
            ResolvedPackage(
                relpath=relpath,
                target_name=target_name,
                package_name=package_name,
                url=url,
                sha256=sha256,
                strip_prefix=info.strip_prefix,
                bin=info.bin,
            )
        )

    buck_file = render_buck_file(resolved, str(args.lockfile))
    args.out_dir.mkdir(parents=True, exist_ok=True)
    out_path = args.out_dir / "BUCK"
    out_path.write_text(buck_file)

    print(f"npm_buckify: wrote {len(resolved)} packages to {out_path}", file=sys.stderr)


if __name__ == "__main__":
    main()
