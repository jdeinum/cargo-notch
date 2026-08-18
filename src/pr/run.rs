use crate::commit;
use crate::config;
use crate::error::{Error, Result};
use crate::pr::git::open_pr;
use crate::utils::git::push_current_branch;
use anyhow::Context;
use secrecy::ExposeSecret;

/// Runs `notch pr`: does everything `notch commit` does (see `commit::commit`), then pushes the
/// branch and opens a release PR for it.
pub fn run(auto: bool, dry_run: bool) -> Result<()> {
    // load the config
    let config = config::load().context("load notch.toml")?;

    // validate we have a github PAT to work with
    // NOTE: This is better than requiring the token as a cli arg because we can pass it as an env
    // override, which avoids poluting your shell history. Checked before `commit` does any work,
    // rather than after, so a missing token fails fast instead of after committing locally.
    let Some(token) = config.repo.token.clone() else {
        return Err(Error::msg("No token provided"));
    };

    // a dry run stops at the plan: no bump, no commit, and so nothing to push or open a PR for.
    // The token check above still applies — a real run needs it, so failing fast on a missing one
    // is more useful than reporting a plan that couldn't have been carried out anyway.
    if dry_run {
        if let Some(plan) = commit::plan(&config, auto).context("plan the release")? {
            commit::describe(&plan);
            println!(
                "would then push the current branch to {} and open a release PR against {}",
                config.release.remote, config.release.default_branch
            );
        }
        return Ok(());
    }

    // the same two halves `notch commit` runs, just with a push and a PR on the end — see
    // `commit::plan` / `commit::apply`
    let Some(plan) = commit::plan(&config, auto).context("plan the release")? else {
        return Ok(());
    };
    let (repo, res) = commit::apply(&config, plan).context("apply the release")?;

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
