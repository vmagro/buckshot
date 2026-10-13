//! buckshot's own maintenance CLI -- generators for the `BUCK` files this
//! repo needs before it can build itself (npm third-party vendoring, the
//! rust toolchain), grouped under one binary so they share a common
//! dependency set instead of each being its own crate.

mod buck;
mod cxx_toolchain;
mod dotslash;
mod node;
mod python;
mod releases;
mod rust_toolchain;
mod update;

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
    /// Update buckshot's own CLI dotslash manifest to a release tag
    Update(update::UpdateArgs),
    /// buck2 itself (dotslash manifests vendored from releases)
    Buck {
        #[command(subcommand)]
        command: BuckCommand,
    },
    /// rust toolchain generation
    Rust {
        #[command(subcommand)]
        command: RustCommand,
    },
    /// cxx toolchain generation
    Cxx {
        #[command(subcommand)]
        command: CxxCommand,
    },
    /// Python generation
    Python {
        #[command(subcommand)]
        command: PythonCommand,
    },
    /// node management
    Node {
        #[command(subcommand)]
        command: node::NodeCommand,
    },
}

#[derive(Subcommand)]
enum BuckCommand {
    /// Update vendored buck2/tools dotslash manifests to a release tag
    Update(buck::UpdateArgs),
}

#[derive(Subcommand)]
enum RustCommand {
    /// Generate a downloaded_rust_toolchain BUCK file from a rustup channel TOML
    Toolchain(rust_toolchain::ToolchainArgs),
    /// Generate rust-analyzer's rust-project.json for buck targets using the
    /// buck-built sysroot (wraps upstream rust-project)
    RustProject(rust_toolchain::RustProjectArgs),
}

#[derive(Subcommand)]
enum CxxCommand {
    /// Generate a zig_cxx_toolchain BUCK file from the Zig download index
    Toolchain(cxx_toolchain::ToolchainArgs),
}

#[derive(Subcommand)]
enum PythonCommand {
    /// Generate a python toolchain BUCK file
    Toolchain(python::toolchain::ToolchainArgs),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Update(args) => update::run(args).await,
        Command::Buck {
            command: BuckCommand::Update(args),
        } => buck::run(args).await,
        Command::Rust {
            command: RustCommand::Toolchain(args),
        } => rust_toolchain::generate(args).await,
        Command::Rust {
            command: RustCommand::RustProject(args),
        } => rust_toolchain::rust_project(args),
        Command::Cxx {
            command: CxxCommand::Toolchain(args),
        } => cxx_toolchain::generate(args).await,
        Command::Python {
            command: PythonCommand::Toolchain(args),
        } => python::toolchain::generate(args).await,
        Command::Node { command } => command.run().await,
    }
}
