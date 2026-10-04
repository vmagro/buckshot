"""Wrapper around the rustc-family compilers for the hermetic toolchain.

Built as a `python_bootstrap_binary` (`:rustc_wrapper`) and invoked as:

    rustc_wrapper.py <zig> <real compiler> [args...]

On POSIX hosts it additionally provisions the `dlltool` shims rustc needs
for `windows-gnu` targets: rustc compiles `#[link(kind = "raw-dylib")]`
externs (used pervasively by `windows-link` / `windows-sys`, hence by most
crates that touch Win32) by shelling out to a target-prefixed binutils
`dlltool` found on `PATH`. The wrapper writes `#!/bin/sh` trampolines of
that exact name (forwarding to `zig dlltool`, the same approach as
`cargo-zigbuild`) into a fresh temp dir and prepends it to `PATH` before
exec'ing the real compiler. All embedded paths are absolutized: rustc
spawns dlltool from a scratch cwd, where buck2's relative action paths
would no longer resolve.

Windows hosts skip the shims (an extensionless `sh` trampoline is not
executable via `CreateProcess`, and rustc only probes `<name>[.exe]`), so
windows-*host* builds of `raw-dylib` crates remain unsupported; the
compiler itself still runs through this wrapper unchanged.
"""

import os
import subprocess
import sys
import tempfile

# Target-prefixed binutils names rustc looks up for `raw-dylib` import-lib
# generation on windows-gnu targets. Only x86_64 exists in
# `platforms/configs` today; aarch64 rides along for free.
DLLTOOL_SHIM_NAMES = (
    "x86_64-w64-mingw32-dlltool",
    "aarch64-w64-mingw32-dlltool",
)

SHIM_TEMPLATE = "#!/bin/sh\nexec \"%s\" dlltool \"$@\"\n"


def scratch_dir():
    # `BUCK_SCRATCH_PATH` arrives via the environment, so unlike command
    # args it gets no `${..}/` substitution when the wrapper runs from a
    # foreign cwd (a cargo build script's `OUT_DIR` invoking `$RUSTC`);
    # fall back to the system temp dir when it does not resolve.
    scratch = os.environ.get("BUCK_SCRATCH_PATH")
    if scratch and os.path.isdir(scratch):
        return scratch
    return tempfile.gettempdir()


def provision_dlltool_shims(zig):
    # Unconditional: only `windows-gnu` rustc invocations ever look up
    # these names, and provisioning is two tiny files -- while gating on
    # `--target` is unreliable (the prelude may pass it inside a response
    # file rather than argv). `zig` is embedded absolutized against the
    # wrapper's own cwd (valid in every context: regular actions run from
    # the project root, and `$RUSTC` shims arrive cwd-correct via
    # `from_any_dir`'s walk-up substitution), because rustc spawns dlltool
    # from a scratch cwd where relative paths would break.
    shim_dir = tempfile.mkdtemp(prefix="rustc-dlltool-", dir=scratch_dir())
    body = SHIM_TEMPLATE % os.path.abspath(zig)
    for name in DLLTOOL_SHIM_NAMES:
        path = os.path.join(shim_dir, name)
        with open(path, "w", newline="\n") as f:
            f.write(body)
        os.chmod(path, 0o755)
    os.environ["PATH"] = shim_dir + os.pathsep + os.environ.get("PATH", "")


def main():
    zig, compiler, args = sys.argv[1], sys.argv[2], sys.argv[3:]
    if os.name == "posix":
        provision_dlltool_shims(zig)
        os.execv(compiler, [compiler] + args)
    else:
        completed = subprocess.run([compiler] + args)
        return completed.returncode
    return 0


if __name__ == "__main__":
    sys.exit(main())
