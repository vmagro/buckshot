// Links ring's Windows ARM64 sources into a foreign binary: the digest call
// forces ring's rlib -- and its `ring-c-win-aarch64` static archive -- into
// the final link, which is what failed before the toolchain's
// `aarch64-windows-gnu` flags and ring's `windows-arm64-gnu` platform deps.
fn main() {
    let digest = ring::digest::digest(&ring::digest::SHA256, b"windows-arm64");
    assert_eq!(digest.as_ref().len(), 32);
    println!("sha256 ok");
}
