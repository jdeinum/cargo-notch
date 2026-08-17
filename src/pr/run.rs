use crate::commit;
use crate::config;
use crate::error::{Error, Result};
use crate::pr::git::open_pr;
use crate::utils::git::push_current_branch;
use anyhow::Context;
use secrecy::ExposeSecret;

/// Runs `notch pr`: does everything `notch commit` does (see `commit::commit`), then pushes the
/// branch and opens a release PR for it.
pub fn run(auto: bool) -> Result<()> {
    // load the config
    let config = config::load().context("load notch.toml")?;

    // validate we have a github PAT to work with
    // NOTE: This is better than requiring the token as a cli arg because we can pass it as an env
    // override, which avoids poluting your shell history. Checked before `commit` does any work,
    // rather than after, so a missing token fails fast instead of after committing locally.
    let Some(token) = config.repo.token.clone() else {
        return Err(Error::msg("No token provided"));
    };

    let Some((repo, res)) = commit::commit(&config, auto).context("run commit")? else {
        return Ok(());
    };

    // push to the remote
    push_current_branch(&repo, &config).context("push current branch")?;

    // open the PR
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("spawn runtime")?;

    rt.block_on(open_pr(&repo, &config, token.expose_secret(), &res))
        .context("open PR on runtime")?;

    Ok(())
}
