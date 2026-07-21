pub(crate) mod cli;
pub(crate) mod cmd;
pub(crate) mod config;
pub(crate) mod error;
pub(crate) mod package;
pub(crate) mod pr;
pub(crate) mod tag;

use crate::{
    cli::{CargoCli, Commands},
    error::Result,
};
use anyhow::Context;
use clap::Parser;

/// Parses CLI arguments and dispatches to the requested subcommand.
/// Returns an error if the underlying `pr` or `tag` command fails.
///
/// # Errors
///
/// Errors out when either command does not succeed
pub fn run() -> Result<()> {
    let CargoCli::Notch(cli) = CargoCli::parse();
    match cli.command {
        Commands::Pr { auto } => pr::run(auto).context("run pr"),
        Commands::Tag { old, new } => tag::run(&old, &new).context("run tag"),
    }
}
