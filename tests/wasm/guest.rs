//! Guest module for the wasm tests: plain exported functions over `i32`s,
//! so `wasmtime run --invoke` can call them without any WASI imports.
//!
//! The arithmetic lives in the `:math` dependency, proving deps link into
//! the final module.

#[unsafe(no_mangle)]
pub extern "C" fn add(a: i32, b: i32) -> i32 {
    math::add(a, b)
}

#[unsafe(no_mangle)]
pub extern "C" fn fib(n: i32) -> i32 {
    math::fib(n)
}
