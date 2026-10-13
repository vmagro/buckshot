#!/usr/bin/env python3
"""Build dotslash release assets for the buckshot CLI.

CI release job usage:
  publish.py asset --triple <t> --key <dotslash-platform> --bin <path> \
      --tag <tag> --out-dir <dir>
  publish.py manifest --tag <tag> --fragments <dir> --out <path>

Requires pip packages: zstandard, blake3.
"""

import argparse
import json
from pathlib import Path

RELEASE_BASE = "https://github.com/vmagro/buckshot/releases/download"


def cmd_asset(a: argparse.Namespace) -> None:
    import blake3
    import zstandard

    raw = Path(a.bin).read_bytes()
    blob = zstandard.compress(raw)
    inner = f"buckshot-{a.triple}" + (".exe" if a.bin.endswith(".exe") else "")
    asset = inner + ".zst"
    (Path(a.out_dir) / asset).write_bytes(blob)
    frag = {
        "key": a.key,
        "size": len(blob),
        "hash": "blake3",
        "digest": blake3.blake3(blob).hexdigest(),
        "format": "zst",
        "path": inner,
        "url": f"{RELEASE_BASE}/{a.tag}/{asset}",
    }
    (Path(a.out_dir) / f"fragment-{a.key}.json").write_text(
        json.dumps(frag, indent=2) + "\n"
    )
    print(f"{asset}: {len(blob)} bytes")


def cmd_manifest(a: argparse.Namespace) -> None:
    plats = {}
    for f in sorted(Path(a.fragments).glob("fragment-*.json")):
        d = json.loads(f.read_text())
        plats[d.pop("key")] = {
            "size": d["size"],
            "hash": d["hash"],
            "digest": d["digest"],
            "format": d["format"],
            "path": d["path"],
            "providers": [{"url": d["url"]}],
        }
    if not plats:
        raise SystemExit("no fragments found")
    doc = "#!/usr/bin/env dotslash\n\n" + json.dumps(
        {"name": "buckshot", "platforms": plats}, indent=2
    ) + "\n"
    Path(a.out).write_text(doc)
    print(f"wrote {a.out} ({len(plats)} platforms)")


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__)
    sub = p.add_subparsers(required=True)
    a = sub.add_parser("asset", help="compress + hash one platform binary")
    a.add_argument("--triple", required=True)
    a.add_argument("--key", required=True)
    a.add_argument("--bin", required=True)
    a.add_argument("--tag", required=True)
    a.add_argument("--out-dir", required=True)
    a.set_defaults(func=cmd_asset)
    m = sub.add_parser("manifest", help="merge fragments into a dotslash file")
    m.add_argument("--tag", required=True)
    m.add_argument("--fragments", required=True)
    m.add_argument("--out", required=True)
    m.set_defaults(func=cmd_manifest)
    args = p.parse_args()
    Path(args.out_dir if hasattr(args, "out_dir") else ".").mkdir(
        parents=True, exist_ok=True
    )
    args.func(args)


if __name__ == "__main__":
    main()
