use crate::{config::ReleaseConfig, error::Result};
use cargo_metadata::semver::Version;
use git2::Repository;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Eq, PartialEq, Hash)]
pub struct Package {
    pub name: String,
    pub path: String,
    pub version: Version,
}

impl Package {
    pub fn bump_patch(&self) -> Version {
        let mut new = self.version.clone();
        new.patch += 1;
        new
    }

    pub fn bump_minor(&self) -> Version {
        let mut new = self.version.clone();
        new.minor += 1;
        new
    }

    pub fn bump_major(&self) -> Version {
        let mut new = self.version.clone();
        new.major += 1;
        new
    }
}

#[derive(Debug)]
pub struct CommitInfo {
    pub summary: String,
    pub sha1: String,
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
