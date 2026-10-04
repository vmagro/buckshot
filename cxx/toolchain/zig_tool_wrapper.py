"""Wrapper around `zig <subcommand>` for the hermetic cxx toolchain.

Built as a `python_bootstrap_binary` (`:zig_tool_wrapper`) and invoked as:

    zig_tool_wrapper.py <zig> <subcommand> [args...]

It structurally flattens nested `@response-files` before invoking Zig:

buck2's compile/link actions pass flags via `@argsfile` response files that
*nest* (the outer file references per-category `@layer` files -- toolchain
args, deps args, ...). Plain clang expands these recursively, but `zig cc`
only expands one level and then either errors (`NestedResponseFile`) or
hands the inner `@file` to clang as an input file. So every `@arg` naming
an existing file is expanded here -- its lines spliced verbatim (recursing
into nested `@file` lines) into a temp flat file -- and Zig gets `@tmp`
instead. An `@arg` is only treated as a file when it exists on disk, so
option values like macOS `@loader_path`/`@rpath` pass through untouched.

The wrapper also drops a few exact args Zig's bundled linkers reject:

- `-Wl,-oso_prefix,.`: buck2 adds it to every Darwin link (to relativize
  N_OSO debug paths). Only affects debug-path metadata, never codegen.
- `-lmsvcrt`: rustc passes it for every `windows-gnu` link, but Zig has
  no such library (its C runtime links UCRT intrinsically, even under
  `-nodefaultlibs`, so the flag is redundant).
- `-l:libpthread.a`: rustc passes it for `windows-gnu`, but Zig ships no
  winpthreads static lib and `std` needs no pthread symbols. A future
  crate that really uses pthreads would fail loudly with undefined
  references instead of silently mislinking.
- `-Wl,--fix-cortex-a53-843419`: rustc passes it for `aarch64-linux`;
  an optional CPU-erratum workaround gcc does not enable by default
  either (same practice as `cargo-zigbuild`, which filters it too).

Response files are processed as bytes (like the `sh` implementation this
replaces), so non-UTF-8 paths round-trip losslessly. One trailing `\r` per
line is stripped so CRLF-authored files work; LF files are unaffected.
"""

import os
import subprocess
import sys
import tempfile

# Exact args Zig's bundled linkers reject (see module docstring). Compared
# as bytes against both argv entries and response-file lines.
DROPPED_ARGS = frozenset(
    [
        b"-Wl,-oso_prefix,.",
        b"-lmsvcrt",
        b"-l:libpthread.a",
        b"-Wl,--fix-cortex-a53-843419",
    ]
)


def read_response_lines(path):
    """Lines of a response file as bytes, without line terminators."""
    with open(path, "rb") as f:
        content = f.read()
    lines = content.split(b"\n")
    if lines and lines[-1] == b"":
        lines.pop()
    return [line[:-1] if line.endswith(b"\r") else line for line in lines]


def flatten_file(path, seen):
    """Yield a response file's lines, recursing into nested `@file` lines."""
    real = os.path.realpath(path)
    if real in seen:
        raise RuntimeError("cyclic @response-file reference: %r" % (path,))
    seen.add(real)
    try:
        for line in read_response_lines(path):
            if line in DROPPED_ARGS:
                continue
            if line.startswith(b"@") and len(line) > 1 and os.path.isfile(line[1:]):
                for nested in flatten_file(line[1:], seen):
                    yield nested
            else:
                yield line
    finally:
        seen.discard(real)


def scratch_dir():
    # `BUCK_SCRATCH_PATH` arrives via the environment, so unlike command
    # args it gets no `${..}/` substitution when the wrapper runs from a
    # foreign cwd (a cargo build script's `OUT_DIR` invoking `$CC`);
    # fall back to the system temp dir when it does not resolve.
    scratch = os.environ.get("BUCK_SCRATCH_PATH")
    if scratch and os.path.isdir(scratch):
        return scratch
    return tempfile.gettempdir()


def main():
    zig, sub, args = sys.argv[1], sys.argv[2], sys.argv[3:]
    scratch = scratch_dir()
    temps = []
    try:
        flat = []
        for arg in args:
            raw = os.fsencode(arg)
            if raw in DROPPED_ARGS:
                continue
            # `@arg` names a response file only when it exists on disk
            # (regular actions run from the project root; `$CC` shims
            # arrive cwd-correct via `from_any_dir`'s walk-up
            # substitution), so values like `@loader_path` pass through.
            if raw.startswith(b"@") and len(raw) > 1 and os.path.isfile(raw[1:]):
                fd, tmp = tempfile.mkstemp(prefix="zigflat.", dir=scratch)
                temps.append(tmp)
                with os.fdopen(fd, "wb") as f:
                    for line in flatten_file(raw[1:], set()):
                        f.write(line + b"\n")
                flat.append("@" + tmp)
            else:
                flat.append(arg)
        cmd = [zig, sub] + flat
        if os.name == "posix":
            os.execv(zig, cmd)
        else:
            # `os.execv` is emulated via spawn+wait on Windows anyway;
            # `subprocess` is the honest spelling (and argv passes as a
            # list -- no cmd.exe quoting hazards at all).
            completed = subprocess.run(cmd)
            return completed.returncode
    finally:
        for tmp in temps:
            try:
                os.unlink(tmp)
            except OSError:
                pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
