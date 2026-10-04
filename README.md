# buckshot

`buckshot` is a collection of buck2 helpers that are opinionated for my personal
use. It is in no way endorsed by the buck2 project.

It primarily aims to make toolchain setup easier across different projects,
including (semi-)hermetic toolchains for Rust, and C++, as well as portable
Python installations.

This repo also includes rules for things not covered by the buck2 prelude (used
here as a `bundled` external cell -- see `.buckconfig`) such as `node`/`npm`
and `protobuf`.