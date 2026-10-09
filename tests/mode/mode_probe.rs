//! Build-mode probe: exits 0 iff the optimization level rustc actually
//! used matches the mode on argv[1]. `cfg!(debug_assertions)` is the
//! observable: the toolchain passes `-Cdebug-assertions=off` (plus
//! `-Copt-level=3`) on `buckshot//mode:mode[release]` and nothing
//! otherwise, so a debug-built binary reports `debug` and a
//! release-built one reports `release`.

fn main() {
    let expected = std::env::args().nth(1).unwrap_or_default();
    let actual = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    if expected != actual {
        eprintln!("mode_probe: expected {expected}, binary built {actual}");
        std::process::exit(1);
    }
    println!("mode_probe: ok ({actual})");
}
