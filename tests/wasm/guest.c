// Guest module for the wasm C tests: plain exported functions over ints,
// so `wasmtime run --invoke` can call them without any WASI imports.
// Linked with `--no-entry --export-all` (see `cxx/toolchain/zig_tool_wrapper.py`),
// so every symbol here is callable -- the `c_` prefix just keeps the
// test names distinct from the rust guest's.
int c_add(int a, int b) {
    return a + b;
}

int c_fib(int n) {
    if (n <= 1) {
        return n;
    }
    return c_fib(n - 1) + c_fib(n - 2);
}
