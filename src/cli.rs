use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "cargo", bin_name = "cargo")]
pub enum CargoCli {
    #[command(version, about = "Version and release automation for cargo workspaces")]
    Notch(Cli),
}

#[derive(Args)]
pub struct Cli {
    #[command(subcommand)]
    pub(crate) command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
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
