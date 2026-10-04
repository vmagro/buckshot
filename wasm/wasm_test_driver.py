"""Assertion driver for `wasm_test` (see `wasm/wasm_test.bzl`).

Runs one `wasmtime run --invoke` case and checks its stdout. Stdlib only:
runs as a `python_bootstrap_binary` on the hermetic bootstrap interpreter.

Usage:
    wasm_test_driver.py <wasmtime> <module> <func> [<func-args>...] -- <expected-stdout>
"""

import subprocess
import sys


def main() -> int:
    argv = sys.argv[1:]
    try:
        sep = argv.index("--")
    except ValueError:
        print("usage: wasm_test_driver.py <wasmtime> <module> <func> [args...] -- <expected>")
        return 2
    if sep < 3:
        print("usage: wasm_test_driver.py <wasmtime> <module> <func> [args...] -- <expected>")
        return 2
    wasmtime, module, func = argv[0], argv[1], argv[2]
    func_args = argv[3:sep]
    expected = argv[sep + 1] if sep + 1 < len(argv) else ""
    if sep + 2 < len(argv):
        print("expected exactly one argv after `--`, got %d" % (len(argv) - sep - 1))
        return 2

    proc = subprocess.run(
        [wasmtime, "run", "--invoke", func, module, *func_args],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    actual = proc.stdout.strip()
    if proc.returncode != 0 or actual != expected.strip():
        print("wasmtime exited %d" % proc.returncode)
        print("--- stdout ---")
        print(proc.stdout, end="" if proc.stdout.endswith("\n") else "\n")
        print("--- stderr ---")
        print(proc.stderr, end="" if proc.stderr.endswith("\n") else "\n")
        print("--- expected stdout %r, got %r ---" % (expected.strip(), actual))
        return 1
    print("ok: %s(%s) == %s" % (func, ", ".join(func_args), actual))
    return 0


if __name__ == "__main__":
    sys.exit(main())
