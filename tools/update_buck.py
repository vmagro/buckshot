#!/usr/bin/env python3
"""Update everything this repo vendors from a facebook/buck2 GitHub release.

Keeps these in sync with one release tag (e.g. 2026-10-01):

  - buck2                      dotslash manifest for the buck2 binary
  - tools/buck/starlark_fmt    dotslash manifest for the Starlark formatter
  - tools/buck/rust-project    dotslash manifest for rust-project

The dotslash files are fetched byte-for-byte from the release's own assets
and validated (shebang, JSON body, `name` field, per-platform URLs) before
they are written. The prelude is not vendored: `.buckconfig` declares it as
a `bundled` external cell, so it always comes with the buck2 binary as the
tested-together pair.

Usage:
    python3 tools/update_buck.py 2026-10-01   # update to an explicit tag
    python3 tools/update_buck.py --latest     # newest YYYY-MM-DD release
    python3 tools/update_buck.py              # same as --latest
"""

import argparse
import json
import re
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

RELEASE_BASE = "https://github.com/facebook/buck2/releases/download"
RELEASES_API = "https://api.github.com/repos/facebook/buck2/releases?per_page=30"

# Release asset name -> path in this repo, in update order.
DOTSLASH_FILES = {
    "buck2": Path("buck2"),
    "starlark_fmt": Path("tools/buck/starlark_fmt"),
    "rust-project": Path("tools/buck/rust-project"),
}

DATE_TAG = re.compile(r"^\d{4}-\d{2}-\d{2}$")
USER_AGENT = "buckshot-tools-update-buck/1.0"


def fetch(url: str, timeout: int = 300) -> bytes:
    req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.read()
    except urllib.error.HTTPError as e:
        raise SystemExit(f"error: {url}: HTTP {e.code} {e.reason}") from e
    except urllib.error.URLError as e:
        raise SystemExit(f"error: {url}: {e.reason}") from e


def resolve_latest() -> str:
    """Newest YYYY-MM-DD buck2 release tag.

    Skips the rolling `latest` release: it ships only `.zst` blobs (no
    dotslash manifests), so it can never be a vendor source here.
    """
    try:
        releases = json.loads(fetch(RELEASES_API, timeout=60))
    except SystemExit as e:
        raise SystemExit(
            f"{e}\nnote: pass an explicit tag instead, e.g. "
            "`python3 tools/update_buck.py 2026-10-01`"
        ) from e
    for release in releases:
        tag = release.get("tag_name", "")
        if DATE_TAG.match(tag):
            return tag
    raise SystemExit("error: no YYYY-MM-DD tag in the recent facebook/buck2 releases")


def validate_dotslash(data: bytes, name: str, tag: str) -> bytes:
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError as e:
        raise SystemExit(
            f"error: release {tag} asset {name!r} is not UTF-8 text: {e}"
        ) from e
    shebang, nl, body = text.partition("\n")
    if shebang != "#!/usr/bin/env dotslash" or not nl:
        raise SystemExit(
            f"error: release {tag} asset {name!r} has no dotslash shebang "
            "(is it still published as a dotslash manifest?)"
        )
    try:
        manifest = json.loads(body)
    except json.JSONDecodeError as e:
        raise SystemExit(
            f"error: release {tag} asset {name!r} has an unparsable manifest: {e}"
        ) from e
    if manifest.get("name") != name:
        raise SystemExit(
            f"error: release {tag} asset {name!r} manifests as "
            f"{manifest.get('name')!r}"
        )
    try:
        platforms = manifest["platforms"]
    except KeyError as e:
        raise SystemExit(
            f"error: release {tag} asset {name!r} has no 'platforms' object"
        ) from e
    for platform, entry in platforms.items():
        url = entry["providers"][0]["url"]
        if f"/{tag}/" not in url:
            raise SystemExit(
                f"error: release {tag} asset {name!r} platform {platform!r} "
                f"points outside the release: {url}"
            )
    return data


def update_dotslash(tag: str) -> bool:
    """Fetch and validate the release's dotslash manifests. True if changed."""
    changed = False
    for name, relpath in DOTSLASH_FILES.items():
        data = validate_dotslash(fetch(f"{RELEASE_BASE}/{tag}/{name}"), name, tag)
        path = ROOT / relpath
        if path.exists() and path.read_bytes() == data:
            print(f"{relpath}: already current")
            continue
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        path.chmod(0o755)
        print(f"{relpath}: updated to {tag}")
        changed = True
    return changed



def main() -> None:
    parser = argparse.ArgumentParser(
        description="Update vendored buck2 and tools to a release tag."
    )
    parser.add_argument(
        "tag",
        nargs="?",
        help="release tag, e.g. 2026-10-01 (default: newest YYYY-MM-DD release)",
    )
    parser.add_argument(
        "--latest",
        action="store_true",
        help="resolve the newest YYYY-MM-DD release (the default when no tag)",
    )
    args = parser.parse_args()

    if args.tag and args.latest:
        raise SystemExit("error: pass a tag or --latest, not both")
    tag = args.tag or resolve_latest()
    print(f"updating to facebook/buck2 release {tag}")

    dotslash_changed = update_dotslash(tag)

    if dotslash_changed:
        print("done -- review with `sl status`")
    else:
        print("done -- already current")


if __name__ == "__main__":
    main()
