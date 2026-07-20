use crate::config::{self, ReleaseConfig};
use crate::error::{Error, Result};
use crate::package::{Package, get_cleaned_members};
use anyhow::Context;
use cargo_metadata::semver::Version;
use git2::{BranchType, Repository, WorktreePruneOptions, build::CheckoutBuilder};
use std::collections::{HashMap, HashSet};
use tracing::debug;

// determine the tag to be created
pub fn tag(old: Option<Version>, new: Option<Version>) -> Result<Option<Version>> {
    match (old, new) {
        // if there is no old version, but a new version, return
        (None, Some(n)) => Ok(Some(n)),

        // if there is both an old and new version, and the new is newer than the old, return new
        (Some(o), Some(n)) if o < n => Ok(Some(n)),

        // if there is both an old and new version, but the older version is higher than the new
        // one, we return an error for that
        (Some(o), Some(n)) if o > n => Err(Error::msg("New version is older than the old version")),

        // otherwise, we don't release anything
        _ => Ok(None),
    }
}

pub fn run(old_commit: &str, new_commit: &str) -> Result<()> {
    let config = config::load().context("load notch.toml")?;

    let repo = Repository::open(".").context("open repo")?;
    let members_source = WorktreeMembers::new(repo);

    let old_members = members_source
        .get(old_commit)
        .context("get crate members")?;
    let new_members = members_source
        .get(new_commit)
        .context("get crate members")?;

    for tag_name in compute_tags(&old_members, &new_members, &config.release)? {
        println!("{tag_name}");
    }

    Ok(())
}

/// Reads a workspace's Cargo package versions as they existed at a given commit —
/// the one piece of `tag`'s logic that actually needs git, kept behind a trait so
/// `compute_tags`/`tag` (the actual version-diffing rules) stay git-agnostic and
/// easy to test without a real repo.
pub trait MembersAtCommit {
    fn get(&self, commit: &str) -> Result<Vec<Package>>;
}

/// Reads workspace members at a commit by checking it out into a throwaway git worktree,
/// without disturbing the caller's current checkout.
pub struct WorktreeMembers {
    repo: Repository,
}

impl WorktreeMembers {
    pub const fn new(repo: Repository) -> Self {
        Self { repo }
    }
}

impl MembersAtCommit for WorktreeMembers {
    fn get(&self, commit: &str) -> Result<Vec<Package>> {
        let oid = self
            .repo
            .revparse_single(commit)
            .context("resolve commit")?
            .peel_to_commit()
            .context("peel to commit")?
            .id();

        // name has to be unique per commit so concurrent/old worktrees don't collide
        let name = format!("notch-tag-{oid}");
        let path = std::env::temp_dir().join(&name);

        let worktree = self
            .repo
            .worktree(&name, &path, None)
            .context("create worktree")?;

        let result = (|| -> Result<Vec<Package>> {
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

        if let Ok(mut branch) = self.repo.find_branch(&name, BranchType::Local) {
            branch.delete().context("delete scratch worktree branch")?;
        }

        result
    }
}

// determines, for each workspace member present in either commit, whether a
// tag should be created, and formats it using the real Cargo package name
// (not the workspace-relative directory, which may differ — see package.rs)
fn compute_tags(
    old_members: &[Package],
    new_members: &[Package],
    release: &ReleaseConfig,
) -> Result<Vec<String>> {
    // keyed by workspace-relative path (stable identity across commits; the
    // Cargo package name is only needed for the emitted tag)
    let old_packages: HashMap<&str, (&str, Version)> = old_members
        .iter()
        .map(|x| (x.path.as_str(), (x.name.as_str(), x.version.clone())))
        .collect();
    let new_packages: HashMap<&str, (&str, Version)> = new_members
        .iter()
        .map(|x| (x.path.as_str(), (x.name.as_str(), x.version.clone())))
        .collect();

    let paths: HashSet<&str> = old_packages
        .keys()
        .chain(new_packages.keys())
        .copied()
        .collect();

    let mut tags = Vec::new();
    for path in paths {
        let old_entry = old_packages.get(path);
        let new_entry = new_packages.get(path);

        let package_name = new_entry
            .or(old_entry)
            .map(|(name, _)| (*name).to_string())
            .ok_or_else(|| Error::msg("No package name"))?;
        let old_version = old_entry.map(|(_, v)| v.clone());
        let new_version = new_entry.map(|(_, v)| v.clone());

        if let Some(tag) = tag(old_version, new_version).context("get tag")? {
            let tag_name = release.format_tag(&package_name, &tag.to_string());
            debug!("creating tag {tag_name} for package {package_name} ({path})");
            tags.push(tag_name);
        }
    }

    Ok(tags)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    fn member(path: &str, name: &str, version: &str) -> Package {
        Package {
            path: path.to_string(),
            name: name.to_string(),
            version: Version::parse(version).unwrap(),
        }
    }

    #[test]
    fn tag_uses_package_name_not_directory_basename() {
        let release = ReleaseConfig::default();
        let old = vec![member("services/user", "user_service", "0.2.48")];
        let new = vec![member("services/user", "user_service", "0.2.49")];
        let tags = compute_tags(&old, &new, &release).unwrap();
        assert_eq!(tags, vec!["user_service-v0.2.49".to_string()]);
    }

    #[test]
    fn respects_configured_tag_format() {
        let release = ReleaseConfig {
            tag_format: "release/{name}/{version}".to_string(),
            ..ReleaseConfig::default()
        };
        let old = vec![member("services/user", "user_service", "0.2.48")];
        let new = vec![member("services/user", "user_service", "0.2.49")];
        let tags = compute_tags(&old, &new, &release).unwrap();
        assert_eq!(tags, vec!["release/user_service/0.2.49".to_string()]);
    }

    #[test]
    fn no_old_with_new_produces_tag() {
        let release = ReleaseConfig::default();
        let new = vec![member("services/user", "user_service", "0.2.49")];
        let tags = compute_tags(&[], &new, &release).unwrap();
        assert_eq!(tags, vec!["user_service-v0.2.49"]);
    }

    #[test]
    fn old_with_new_produces_tag() {
        let release = ReleaseConfig::default();
        let old = vec![member("services/user", "user_service", "0.2.48")];
        let new = vec![member("services/user", "user_service", "0.2.49")];
        let tags = compute_tags(&old, &new, &release).unwrap();
        assert_eq!(tags, vec!["user_service-v0.2.49"]);
    }

    #[test]
    fn old_with_newer_than_new_produces_error() {
        let release = ReleaseConfig::default();
        let old = vec![member("services/user", "user_service", "0.2.50")];
        let new = vec![member("services/user", "user_service", "0.2.49")];
        let tags = compute_tags(&old, &new, &release);
        assert_matches!(tags, Err(_));
    }

    #[test]
    fn old_with_no_new_produces_no_tag() {
        let release = ReleaseConfig::default();
        let old = vec![member("services/user", "user_service", "0.2.48")];
        let tags: Vec<String> = compute_tags(&old, &[], &release).unwrap();
        assert!(tags.is_empty());
    }
}
