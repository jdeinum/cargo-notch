pub(crate) mod error;
pub(crate) mod pr;
pub(crate) mod tag;
pub(crate) mod workspace;

use clap::{Parser, Subcommand};
use error::Result;

#[derive(Parser)]
#[command(
    name = "notch",
    version,
    about = "Version and release automation for cargo workspaces"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Bump versions, update changelogs, and open a release PR for changed crates
    Pr,

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
    let cli = Cli::parse();
    match cli.command {
        Commands::Pr => pr::run(),
        Commands::Tag { old, new } => tag::run(&old, &new),
    }
}
