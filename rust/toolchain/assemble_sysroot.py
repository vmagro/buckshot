"""Merge rustup component archives into one sysroot dir.

Built as a `python_bootstrap_binary` (`:assemble_sysroot`) and invoked as:

    assemble_sysroot.py <out dir> <part dir>...

Each part (the unpacked rustc archive, the host rust-std archive, the
target rust-std archive, clippy/rustfmt/cargo when included) is copied over
`<out dir>` in order -- the equivalent of `cp -R <part>/. <out>/` per part.
Symlinks are dereferenced (content is copied): `cp -R` preserves them as
links, which Windows hosts cannot create without extra privileges, and
nothing in the sysroot is consumed via its link identity.
"""

import os
import shutil
import sys


def copy_part(src_dir, out_dir):
    for entry in sorted(os.listdir(src_dir)):
        src = os.path.join(src_dir, entry)
        dst = os.path.join(out_dir, entry)
        # `realpath` classifies through symlinks: a link to a dir merges
        # as a dir, a link to a file copies its content.
        if os.path.isdir(os.path.realpath(src)):
            shutil.copytree(src, dst, symlinks=False, dirs_exist_ok=True)
        else:
            if os.path.isdir(dst) and not os.path.islink(dst):
                shutil.rmtree(dst)
            shutil.copy2(src, dst)


def main():
    out = sys.argv[1]
    os.makedirs(out, exist_ok=True)
    for part in sys.argv[2:]:
        copy_part(part, out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
