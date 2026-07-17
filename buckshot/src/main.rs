//! buckshot's own maintenance CLI -- generators for the `BUCK` files this
//! repo needs before it can build itself (npm third-party vendoring, the
//! rust toolchain), grouped under one binary so they share a common
//! dependency set instead of each being its own crate.

mod node_toolchain;
mod npm;
mod python;
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
    /// Python generation
    Python {
        #[command(subcommand)]
        command: PythonCommand,
    },
    /// node toolchain generation
    Node {
        #[command(subcommand)]
        command: NodeCommand,
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

#[derive(Subcommand)]
enum PythonCommand {
    /// Generate a python toolchain BUCK file
    Toolchain(python::toolchain::ToolchainArgs),
}

#[derive(Subcommand)]
enum NodeCommand {
    /// Generate a node toolchain BUCK file from nodejs.org's SHASUMS256.txt
    Toolchain(node_toolchain::ToolchainArgs),
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
        Command::Python {
            command: PythonCommand::Toolchain(args),
        } => python::toolchain::generate(args).await,
        Command::Node {
            command: NodeCommand::Toolchain(args),
        } => node_toolchain::generate(args).await,
    }
}
