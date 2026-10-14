#include <cstdio>
#include <cstring>

// Plain C++ hello world: proves a C++ binary compiles and links through
// `zig c++ -target aarch64-windows-gnu` and lands as a Windows ARM64 `.exe`.
// (The flags regression -- Zig's default UBSan/stack-protector
// instrumentation referencing runtimes lld-link never provides -- is
// covered by `:ring_probe`, whose larger C archive trips it; this TU is
// too small to emit those references.)
int main() {
    char buf[64];
    const char* msg = "hello windows-arm64";
    strcpy(buf, msg);
    int total = 0;
    for (int i = 0; buf[i] != '\0'; ++i) {
        total += buf[i] * i;
    }
    printf("%s %d\n", buf, total);
    return 0;
}
