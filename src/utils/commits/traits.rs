use crate::config::Config;
use crate::error::Result;
use crate::utils::package::Package;
use git2::Repository;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CommitInfo {
    pub summary: String,

    /// The SHA1 of the commit
    /// NOTE: I initially had used [u8;20] to represent SHA1, but granted that the git tooling
    /// allows you to enter an arbitrary length sha1, I figured I should not limit us to the full
    /// hash for now.
    pub sha1: String,
    /// Whether the commit body carries a `BREAKING CHANGE:` /
    /// `BREAKING-CHANGE:` footer. The header's `!` marker is not reflected
    /// here — it stays visible in `summary` and is parsed from there.
    pub breaking: bool,
}

impl CommitInfo {
    #[cfg(test)]
    pub const fn new(summary: String, sha1: String, breaking: bool) -> Self {
        Self {
            summary,
            sha1,
            breaking,
        }
    }

    pub fn short_id(&self) -> &str {
        &self.sha1[0..7]
    }
}

/// Assigns commits to packages
pub trait PackageCommits {
    /// Returns, alongside the per-package commit attribution and the repo handle, the commit
    /// range git-cliff should scan to build the changelog — the release config's configured
    /// range, unnarrowed by any prior notch commit.
    fn get(
        &mut self,
        config: &Config,
        packages: HashSet<Package>,
    ) -> Result<(HashMap<Package, Vec<CommitInfo>>, Repository, String)>;
}
