//! Arithmetic for the wasm guest (`:guest` in `tests/wasm/`).
//!
//! Living in a separate crate (linked via `:guest`'s `deps`) proves that
//! dependencies are linked into the final `.wasm` module: the exported
//! functions below would be undefined symbols otherwise.

pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

pub fn fib(n: i32) -> i32 {
    if n <= 1 {
        n
    } else {
        fib(n - 1) + fib(n - 2)
    }
}
