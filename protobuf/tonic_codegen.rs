//! Tonic codegen invoked by the `protobuf_library` macro's `tonic` language.
//!
//! Takes proto sources + a list of import roots on the CLI, runs
//! `tonic-prost-build` (prost messages + gRPC server) into a tempdir, and
//! emits a single self-contained `lib.rs` containing every package's
//! generated code. Sibling to `proto_codegen.rs`, which emits prost messages
//! only.

mod codegen_common;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use clap::Parser;
use tempfile::tempdir;

#[derive(Parser, Debug)]
#[command(about = "Compile .proto sources into a single tonic lib.rs")]
struct Args {
    #[command(flatten)]
    common: codegen_common::CommonArgs,

    /// Also generate the tonic gRPC client alongside the server. Servers
    /// are the default product; clients exist for round-trip tests.
    #[arg(long)]
    client: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let common = args.common;

    if common.protos.is_empty() {
        bail!("no .proto files provided");
    }

    let tmp = tempdir()?;

    let mut prost_config = prost_build::Config::new();
    prost_config.protoc_executable(&common.protoc);
    codegen_common::apply_extern_paths(&mut prost_config, &common.extern_paths)?;
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(args.client)
        .out_dir(tmp.path())
        .compile_with_config(prost_config, &common.protos, &common.proto_paths)
        .context("tonic codegen failed")?;

    let lib = codegen_common::assemble_lib(tmp.path(), "tonic_codegen")?;
    std::fs::write(&common.out, lib.as_bytes())
        .with_context(|| format!("writing {}", common.out.display()))?;

    Ok(())
}
