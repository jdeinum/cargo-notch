pub(crate) mod cli;
pub(crate) mod commit;
pub(crate) mod config;
pub(crate) mod error;
pub(crate) mod init;
pub(crate) mod pr;
pub(crate) mod tag;
pub(crate) mod utils;

use crate::{
    cli::{CargoCli, Commands},
    error::Result,
};
use anyhow::Context;
use clap::Parser;
use tracing_subscriber::EnvFilter;

/// Parses CLI arguments and dispatches to the requested subcommand.
/// Returns an error if the underlying `pr` or `tag` command fails.
///
/// # Errors
///
/// Errors out when either command does not succeed
pub fn run() -> Result<()> {
    let CargoCli::Notch(cli) = CargoCli::parse();

    // I know its bad practice to set up a subscriber in the lib, but this lib isn't something other
    // people will use so I will stick it here.
    if cli.verbose {
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
            .init();
    }

    match cli.command {
        Commands::Pr { auto } => pr::run(auto).context("run pr"),
        Commands::Tag { old, new } => tag::run(&old, &new).context("run tag"),
        Commands::Init {} => init::run().context("init notch"),
        Commands::Commit { auto } => commit::run(auto).context("run commit"),
    }
}
