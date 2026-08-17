use crate::error::Result;
use anyhow::{Context, Error};
use std::path::Path;
use std::process::{Command, Stdio};

/// Runs a command to completion in the current directory, returning the output / stderr
pub fn run_command(cmd: &[&str]) -> Result<()> {
    run_command_in(Path::new("."), cmd)
}

/// Runs a command to completion in `dir`. Callers that already thread a repo root around (see
/// [`Ecosystem`](crate::utils::packages::Ecosystem)) use this rather than relying on the process
/// happening to be chdir'd into the right place.
pub fn run_command_in(dir: &Path, cmd: &[&str]) -> Result<()> {
    if cmd.is_empty() {
        return Err(Error::msg("Command is empty"));
    }

    let (command, args) = (cmd[0], &cmd[1..]);

    let res = Command::new(command)
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
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
