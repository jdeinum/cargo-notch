pub mod command;
pub mod commits;
pub mod git;
pub mod lock;
pub mod package;
pub mod packages;

use crate::error::Result;
use crate::utils::command::run_command;
use anyhow::Context;

/// update the lockfile for the current project
#[inline]
pub fn generate_lockfile() -> Result<()> {
    run_command(&["cargo", "generate-lockfile"]).context("call cargo generate-lockfile")?;
    Ok(())
}
