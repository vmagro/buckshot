use clap::Subcommand;

mod npm;
mod toolchain;

#[derive(Subcommand)]
pub(crate) enum NodeCommand {
    /// npm third-party management
    #[command(subcommand)]
    Npm(npm::NpmCommand),
    /// Generate a node toolchain BUCK file from nodejs.org's SHASUMS256.txt
    Toolchain(toolchain::ToolchainArgs),
    /// Generate a TypeScript native-package BUCK file from the npm registry
    Typescript(toolchain::TypescriptArgs),
}

impl NodeCommand {
    pub(crate) async fn run(self) -> anyhow::Result<()> {
        match self {
            NodeCommand::Toolchain(args) => toolchain::generate(args).await,
            NodeCommand::Typescript(args) => toolchain::generate_typescript(args).await,
            NodeCommand::Npm(args) => args.run().await,
        }
    }
}
