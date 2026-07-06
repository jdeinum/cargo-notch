use crate::error::{Error, Result};
use anyhow::Context;
use cargo_metadata::{MetadataCommand, semver::Version};
use git2::{BranchType, Repository, WorktreePruneOptions, build::CheckoutBuilder};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    str::FromStr,
};
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

pub fn parse_version(v: &str) -> Result<Version> {
    #[derive(Deserialize)]
    pub struct Manifest {
        version: String,
    }
    let x: Manifest = toml::from_str(v).context("parse manifest")?;
    let version: Version = Version::from_str(&x.version).context("parse version")?;
    Ok(version)
}

pub fn run(old_commit: String, new_commit: String) -> Result<()> {
    // get all of the packages from the old commit
    let old_packages: HashMap<String, Version> = get_cleaned_members_in_commit(&old_commit)
        .context("get crate members")?
        .iter()
        .map(|x| (x.name.clone(), x.version.version.clone()))
        .collect();

    // get all of the packages from the new commit
    let new_packages: HashMap<String, Version> = get_cleaned_members_in_commit(&new_commit)
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
            info!("creating tag {tag} for package");
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct Crate {
    name: String,
    version: MyVersion,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct MyVersion {
    version: Version,
}

impl MyVersion {
    fn bump_patch(&self) -> Version {
        let mut new = self.version.clone();
        new.patch += 1;
        new
    }

    fn bump_minor(&self) -> Version {
        let mut new = self.version.clone();
        new.minor += 1;
        new
    }

    fn bump_major(&self) -> Version {
        let mut new = self.version.clone();
        new.major += 1;
        new
    }
}

fn get_cleaned_members(dir: &Path) -> Result<Vec<Crate>> {
    // get the list of crates in the workspace rooted at `dir`
    let metadata = MetadataCommand::new()
        .current_dir(dir)
        .exec()
        .context("run cargo metadata")?;
    let members = metadata.workspace_members;
    let packages = metadata.packages;
    info!("Members: {members:?}");

    // clean up the members
    let cleaned_members: Vec<Crate> = members
        .iter()
        .map(|s| {
            let x: String = s
                .repr
                .replace("path+file://", "")
                .replace(&format!("{}/", dir.to_str().unwrap()), "")
                .split('#')
                .next()
                .unwrap()
                .to_string();

            let v = packages
                .iter()
                .find(|p| p.id == *s)
                .unwrap()
                .version
                .clone();
            Crate {
                name: x,
                version: MyVersion { version: v },
            }
        })
        .collect();

    info!("cleaned members: {cleaned_members:?}");
    Ok(cleaned_members)
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
