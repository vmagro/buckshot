"""Extract `rust-lld` (+ its libLLVM) from a rustc archive into `<out dir>`.

Built as a `python_bootstrap_binary` (`:extract_rust_lld`) and invoked as:

    extract_rust_lld.py <out dir> <rustc dir> <host triple>

Produces `<out dir>/bin/rust-lld` (or `rust-lld.exe` on Windows hosts)
alongside `<out dir>/lib/libLLVM.*`, mirroring what the old `sh`
implementation copied.
"""

import glob
import os
import shutil
import sys


def main():
    out, rustc, triple = sys.argv[1], sys.argv[2], sys.argv[3]
    bindir = os.path.join(out, "bin")
    libdir = os.path.join(out, "lib")
    os.makedirs(bindir, exist_ok=True)
    os.makedirs(libdir, exist_ok=True)

    lld_dir = os.path.join(rustc, "lib", "rustlib", triple, "bin")
    exe = "rust-lld.exe" if "windows" in triple else "rust-lld"
    src = os.path.join(lld_dir, exe)
    if not os.path.isfile(src):
        raise RuntimeError("no %s in %s" % (exe, lld_dir))
    dst = os.path.join(bindir, exe)
    shutil.copy2(src, dst)
    if os.name == "posix":
        os.chmod(dst, 0o755)

    for lib in sorted(glob.glob(os.path.join(rustc, "lib", "libLLVM.*"))):
        if os.path.isfile(lib):
            shutil.copy2(lib, os.path.join(libdir, os.path.basename(lib)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
