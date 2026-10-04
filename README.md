# buckshot

`buckshot` is a collection of buck2 helpers that are opinionated for my personal
use. It is in no way endorsed by the buck2 project.

It primarily aims to make toolchain setup easier across different projects,
including (semi-)hermetic toolchains for Rust and Python, and hopefully one day
C++.

This repo also includes rules for things not covered by `buck2-prelude` such as
`node`/`npm` and `protobuf`.