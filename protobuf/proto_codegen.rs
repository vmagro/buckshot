//! Proto codegen invoked by the `protobuf_library` macro.
//!
//! Takes proto sources + a list of import roots on the CLI, runs
//! `prost-build` into a tempdir, and emits a single self-contained `lib.rs`
//! containing every package's generated code.

mod codegen_common;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use clap::Parser;
use tempfile::tempdir;

#[derive(Parser, Debug)]
#[command(about = "Compile .proto sources into a single lib.rs")]
struct Args {
    #[command(flatten)]
    common: codegen_common::CommonArgs,
}

fn main() -> Result<()> {
    let common = Args::parse().common;

    if common.protos.is_empty() {
        bail!("no .proto files provided");
    }

    let tmp = tempdir()?;

    let mut prost_config = prost_build::Config::new();
    prost_config.protoc_executable(&common.protoc);
    codegen_common::apply_extern_paths(&mut prost_config, &common.extern_paths)?;
    prost_config
        .out_dir(tmp.path())
        .type_attribute(".", "#[allow(dead_code)]")
        .enable_type_names()
        .compile_protos(&common.protos, &common.proto_paths)
        .context("prost-build compile failed")?;

    let lib = codegen_common::assemble_lib(tmp.path(), "proto_codegen")?;
    std::fs::write(&common.out, lib.as_bytes())
        .with_context(|| format!("writing {}", common.out.display()))?;

    Ok(())
}
