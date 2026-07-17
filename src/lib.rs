pub(crate) mod config;
pub(crate) mod error;
pub(crate) mod pr;
pub(crate) mod tag;
pub(crate) mod workspace;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use error::Result;

#[derive(Parser)]
#[command(name = "cargo", bin_name = "cargo")]
enum CargoCli {
    #[command(version, about = "Version and release automation for cargo workspaces")]
    Notch(Cli),
}

#[derive(Args)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Bump versions, update changelogs, and open a release PR for changed crates
    Pr {
        // We used to have the GitHub token here, but its not good to expose it so we'll move it to
        // the config, and just assert its present when we parse the command
    },
    /// Tag crates whose version changed between two commits
    Tag {
        /// Commit-ish to diff from (the previous release point)
        #[arg(long)]
        old: String,
        /// Commit-ish to diff to (the new release point)
        #[arg(long)]
        new: String,
    },
}

/// Parses CLI arguments and dispatches to the requested subcommand.
///
/// # Errors
///
/// Returns an error if the underlying `pr` or `tag` command fails.
pub fn run() -> Result<()> {
    let CargoCli::Notch(cli) = CargoCli::parse();
    match cli.command {
        Commands::Pr {} => pr::run().context("run pr"),
        Commands::Tag { old, new } => tag::run(&old, &new).context("run tag"),
    }
}
