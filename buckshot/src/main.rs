//! buckshot's own maintenance CLI -- generators for the `BUCK` files this
//! repo needs before it can build itself (npm third-party vendoring, the
//! rust toolchain), grouped under one binary so they share a common
//! dependency set instead of each being its own crate.

mod npm;
mod rust_toolchain;

use clap::Parser;
use clap::Subcommand;

#[derive(Parser)]
#[command(
    name = "buckshot",
    about = "Generators for buckshot's own third-party/toolchain BUCK files"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// npm third-party vendoring
    Npm {
        #[command(subcommand)]
        command: NpmCommand,
    },
    /// rust toolchain generation
    Rust {
        #[command(subcommand)]
        command: RustCommand,
    },
}

#[derive(Subcommand)]
enum NpmCommand {
    /// Generate third-party/npm/BUCK from package-lock.json
    Buckify(npm::BuckifyArgs),
}

#[derive(Subcommand)]
enum RustCommand {
    /// Generate a downloaded_rust_toolchain BUCK file from a rustup channel TOML
    Toolchain(rust_toolchain::ToolchainArgs),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Npm {
            command: NpmCommand::Buckify(args),
        } => npm::buckify(args).await,
        Command::Rust {
            command: RustCommand::Toolchain(args),
        } => rust_toolchain::generate(args).await,
    }
}
