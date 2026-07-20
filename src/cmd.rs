use crate::error::Result;
use anyhow::{Context, Error};
use std::process::Command;

/// Runs a command to completion, returning the output / stderr
pub fn run_command(cmd: &[&str]) -> Result<()> {
    if cmd.is_empty() {
        return Err(Error::msg("Command is empty"));
    }

    let (command, args) = (cmd[0], &cmd[1..]);

    let res = Command::new(command)
        .args(args)
        .spawn()
        .context("spawn child to run command")?
        .wait()
        .context("wait for child")?;

    if !res.success() {
        return Err(Error::msg(format!(
            "Command did not succeed: {:?}",
            res.code()
        )));
    }
    Ok(())
}
