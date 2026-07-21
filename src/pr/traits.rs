use crate::config::ReleaseConfig;
use crate::error::Result;
use crate::package::Package;
use git2::Repository;
use std::collections::{HashMap, HashSet};

#[derive(Debug)]
pub struct CommitInfo {
    pub summary: String,
    pub sha1: String,
    /// Whether the commit body carries a `BREAKING CHANGE:` /
    /// `BREAKING-CHANGE:` footer. The header's `!` marker is not reflected
    /// here — it stays visible in `summary` and is parsed from there.
    pub breaking: bool,
}

impl CommitInfo {
    pub fn short_id(&self) -> &str {
        &self.sha1[0..7]
    }
}

/// Defines our packages in our system
pub trait Packages {
    fn get(&self) -> Result<HashSet<Package>>;
}

/// Assigns commits to packages
pub trait PackageCommits {
    /// Returns, alongside the per-package commit attribution and the repo handle, the commit
    /// range git-cliff should scan to build the changelog (see `pr::changelog_range`) — computed
    /// here rather than by the caller since finding it requires the same commit walk this does.
    fn get(
        &mut self,
        config: &ReleaseConfig,
        packages: HashSet<Package>,
    ) -> Result<(HashMap<Package, Vec<CommitInfo>>, Repository, String)>;
}
