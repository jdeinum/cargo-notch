use crate::error::{Error, Result};
use crate::workspace::{Crate, get_cleaned_members};
use anyhow::Context;
use cargo_metadata::semver::Version;
use git2::{BranchType, Repository, WorktreePruneOptions, build::CheckoutBuilder};
use std::collections::{HashMap, HashSet};
use tracing::info;

// determine the tag to be created
pub fn tag(old: Option<Version>, new: Option<Version>) -> Result<Option<Version>> {
    match (old, new) {
        // if there is no old version, but a new version, return
        (None, Some(n)) => Ok(Some(n)),

        // if there is both an old and new version, and the new is newer than the old, return new
        (Some(o), Some(n)) if o < n => Ok(Some(n)),

        // if there is both an old and new version, but the older version is higher than the new
        // one, we return an error for that
        (Some(o), Some(n)) if o < n => Err(Error::msg("New version is older than the old version")),

        // otherwise, we don't release anything
        _ => Ok(None),
    }
}

pub fn run(old_commit: &str, new_commit: &str) -> Result<()> {
    // get all of the packages from the old commit
    let old_packages: HashMap<String, Version> = get_cleaned_members_in_commit(old_commit)
        .context("get crate members")?
        .iter()
        .map(|x| (x.name.clone(), x.version.version.clone()))
        .collect();

    // get all of the packages from the new commit
    let new_packages: HashMap<String, Version> = get_cleaned_members_in_commit(new_commit)
        .context("get crate members")?
        .iter()
        .map(|x| (x.name.clone(), x.version.version.clone()))
        .collect();

    // get all of the names so we can easily iterate over them
    let names: HashSet<&str> = old_packages
        .iter()
        .chain(new_packages.iter())
        .map(|x| x.0.as_str())
        .collect();

    // for each package:
    for package in names {
        let old_version = old_packages.get(package).cloned();
        let new_version = new_packages.get(package).cloned();

        if let Some(tag) = tag(old_version, new_version).context("get tag")? {
            // derive the name
            let package_name: &str = package
                .rsplit("/")
                .next()
                .ok_or(Error::msg("No package name"))?;

            info!("creating tag {tag} for package {package}");
            println!("{package_name}:{tag}");
        }
    }

    Ok(())
}

// checks out `commit` into a throwaway worktree so we can read the workspace's
// Cargo.toml files as they existed at that point in history, without disturbing
// the caller's current checkout.
fn get_cleaned_members_in_commit(commit: &str) -> Result<Vec<Crate>> {
    let repo = Repository::open(".").context("open repo")?;

    let oid = repo
        .revparse_single(commit)
        .context("resolve commit")?
        .peel_to_commit()
        .context("peel to commit")?
        .id();

    // name has to be unique per commit so concurrent/old worktrees don't collide
    let name = format!("notch-tag-{oid}");
    let path = std::env::temp_dir().join(&name);

    let worktree = repo
        .worktree(&name, &path, None)
        .context("create worktree")?;

    let result = (|| -> Result<Vec<Crate>> {
        let worktree_repo =
            Repository::open_from_worktree(&worktree).context("open worktree repo")?;
        worktree_repo
            .set_head_detached(oid)
            .context("detach worktree head")?;
        worktree_repo
            .checkout_head(Some(CheckoutBuilder::new().force()))
            .context("checkout commit in worktree")?;

        get_cleaned_members(&path).context("get cleaned members from worktree")
    })();

    // always clean up the worktree and its scratch branch, even if the above failed
    let mut prune_opts = WorktreePruneOptions::new();
    prune_opts.valid(true).working_tree(true);
    worktree
        .prune(Some(&mut prune_opts))
        .context("prune worktree")?;

    if let Ok(mut branch) = repo.find_branch(&name, BranchType::Local) {
        branch.delete().context("delete scratch worktree branch")?;
    }

    result
}
