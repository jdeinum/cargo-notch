use crate::config::ReleaseConfig;
use crate::error::{Error, Result};
use crate::pr::git::{changelog_range, is_notch_commit};
use crate::pr::traits::{CommitInfo, Package, PackageCommits};
use anyhow::Context;
use git2::{Commit, Cred, DiffOptions, FetchOptions, Oid, RemoteCallbacks, Repository, Sort};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tracing::debug;

/// Assigns commits to packages by walking the local worktree's git history,
/// rather than e.g. asking a forge API for it.
pub struct WorktreeCommitAssigner {
    repo: Option<Repository>,
}

impl WorktreeCommitAssigner {
    pub const fn new(repo: Repository) -> Self {
        Self { repo: Some(repo) }
    }
}

impl PackageCommits for WorktreeCommitAssigner {
    fn get(
        &mut self,
        config: &ReleaseConfig,
        packages: HashSet<Package>,
    ) -> Result<(HashMap<Package, Vec<CommitInfo>>, Repository, String)> {
        let repo = self
            .repo
            .take()
            .ok_or_else(|| Error::msg("WorktreeCommitAssigner's repo was already taken"))?;

        fetch_remote(&repo, config).context("fetch remote")?;

        // Scoped so every `Commit<'_>` borrowing `repo` (and their `Drop` impls) is gone before
        // `repo` is moved into the return value below.
        let (attributed, changelog_range) = {
            let all_commits = get_commits(&repo, config).context("get commits")?;

            // If a previous run already left a bump commit somewhere in this range (whether or not
            // new commits have since landed on top of it), diff and generate the changelog against
            // it instead of the full upstream range, and only attribute commits newer than it.
            let last_notch_commit = find_last_notch_commit(&all_commits);
            let changelog_range = changelog_range(config, last_notch_commit.as_ref());

            let commits: Vec<Commit> = match &last_notch_commit {
                Some(marker) => all_commits
                    .into_iter()
                    .skip_while(|c| c.id() != marker.id())
                    .skip(1)
                    .collect(),
                None => all_commits,
            };

            let changed = get_changed_packages(&repo, config, packages, last_notch_commit.as_ref())
                .context("get changed packages")?;

            let attributed = attribute_commits_to_packages(&repo, &commits, changed)
                .context("attribute commits to changed packages")?;

            (attributed, changelog_range)
        };

        Ok((attributed, repo, changelog_range))
    }
}

// Updates the local `<remote>/<default_branch>` tracking ref before we diff against it. Without
// this, a stale local ref makes `commit_range()` include far more history than is actually
// unmerged, and every package ever touched in that stale range gets (incorrectly) flagged as changed.
fn fetch_remote(repo: &Repository, release: &ReleaseConfig) -> Result<()> {
    let mut remote = repo
        .find_remote(&release.remote)
        .context("get remote to fetch")?;

    let mut callbacks = RemoteCallbacks::new();
    callbacks.credentials(|_url, username, _allowed| {
        Cred::ssh_key_from_agent(username.unwrap_or("git"))
    });

    let mut opts = FetchOptions::new();
    opts.remote_callbacks(callbacks);

    remote
        .fetch(&[&release.default_branch], Some(&mut opts), None)
        .context("fetch default branch from remote")?;

    Ok(())
}

fn get_commits<'a>(repo: &'a Repository, release: &ReleaseConfig) -> Result<Vec<Commit<'a>>> {
    // find the list of commits present locally but not on the release branch
    let mut revwalk = repo.revwalk().context("create revwalk")?;

    let commit_range = release.commit_range();
    revwalk
        .push_range(&commit_range)
        .context("revwalk commit range")?;
    revwalk.set_sorting(Sort::TOPOLOGICAL | Sort::REVERSE)?;

    let oids: std::result::Result<Vec<Oid>, git2::Error> = revwalk.collect();
    let oids = oids.context("get oids")?;
    let commits: std::result::Result<Vec<Commit>, git2::Error> =
        oids.iter().map(|c| repo.find_commit(*c)).collect();
    let commits = commits.context("get commits from oids")?;

    debug!("Commits: {commits:?}");
    Ok(commits)
}

// `commits` is oldest-first, so the last match is the most recent bump — the one a rerun should
// diff against.
pub(super) fn find_last_notch_commit<'a>(commits: &[Commit<'a>]) -> Option<Commit<'a>> {
    commits.iter().rev().find(|c| is_notch_commit(c)).cloned()
}

// Determines which packages actually differ between HEAD and `base_override` (if given — see
// `find_last_notch_commit`) or otherwise the merge-base with `<remote>/<default_branch>` — i.e.
// what's really different from the current upstream state, regardless of how the branch's history
// got there (merges, reverts, rebases, ...). This is the authoritative source for which packages need
// a version bump.
//
// Walking each commit's diff against its immediate parent and unioning the touched paths doesn't
// work: a merge commit's diff against its first parent pulls in everything the *other* parent
// brought in (e.g. an entire release worth of changes from `master`), and reverted changes still
// get counted even though the net diff against upstream is zero.
fn get_changed_packages(
    repo: &Repository,
    release: &ReleaseConfig,
    packages: HashSet<Package>,
    base_override: Option<&Commit>,
) -> Result<HashSet<Package>> {
    let head = repo
        .head()
        .context("get head")?
        .peel_to_commit()
        .context("peel head to commit")?;

    let base_commit = if let Some(c) = base_override {
        c.clone()
    } else {
        let upstream_ref = format!("{}/{}", release.remote, release.default_branch);
        let upstream = repo
            .revparse_single(&upstream_ref)
            .context("resolve upstream ref")?
            .peel_to_commit()
            .context("peel upstream ref to commit")?;

        let base_oid = repo
            .merge_base(head.id(), upstream.id())
            .context("find merge base with upstream")?;
        repo.find_commit(base_oid)
            .context("find merge base commit")?
    };

    let head_tree = head.tree().context("get head tree")?;
    let base_tree = base_commit.tree().context("get merge base tree")?;
    let diff = repo
        .diff_tree_to_tree(
            Some(&base_tree),
            Some(&head_tree),
            Some(&mut DiffOptions::default()),
        )
        .context("diff head against merge base")?;

    let files: HashSet<&Path> = diff
        .deltas()
        .flat_map(|d| [d.new_file().path().unwrap(), d.old_file().path().unwrap()])
        .collect();

    debug!(
        "Files changed between {} and HEAD: {files:?}",
        base_commit.id()
    );

    let changed: HashSet<Package> = packages
        .into_iter()
        .filter(|p| p.path == "." || files.iter().any(|f| f.starts_with(&p.path)))
        .collect();

    for package in &changed {
        debug!("Package {} changed", package.path);
    }

    Ok(changed)
}

// Attributes each already-confirmed changed package to the non-merge commits whose own diff touched
// its path, purely for displaying to the user which commits are likely responsible — this is
// informational, not authoritative (see `get_changed_packages`).
fn attribute_commits_to_packages(
    repo: &Repository,
    commits: &[Commit],
    changed: HashSet<Package>,
) -> Result<HashMap<Package, Vec<CommitInfo>>> {
    let mut attributed: HashMap<Package, Vec<CommitInfo>> =
        changed.into_iter().map(|p| (p, Vec::new())).collect();

    for commit in commits {
        // merge commits are excluded here for the same reason as in
        // `get_changed_packages`: their diff against a single parent isn't a
        // faithful account of what that commit itself changed.
        if commit.parent_count() != 1 {
            continue;
        }

        let parent_commit = commit.parent(0).context("get first parent")?;
        let cur_tree = commit.tree().context("get tree for current commit")?;
        let parent_tree = parent_commit.tree().context("get tree for parent commit")?;
        let diff = repo
            .diff_tree_to_tree(
                Some(&cur_tree),
                Some(&parent_tree),
                Some(&mut DiffOptions::default()),
            )
            .context("get diff between old and new tree")?;

        let files: HashSet<&Path> = diff
            .deltas()
            .flat_map(|d| [d.new_file().path().unwrap(), d.old_file().path().unwrap()])
            .collect();

        for (package, package_commits) in &mut attributed {
            if package.path == "." || files.iter().any(|f| f.starts_with(&package.path)) {
                package_commits.push(commit.into());
            }
        }
    }

    for (package, commits) in &attributed {
        debug!(
            "Package {} changed from the following commits:",
            package.path
        );
        for c in commits {
            debug!("{}", c.summary);
        }
    }

    Ok(attributed)
}

impl From<&Commit<'_>> for CommitInfo {
    fn from(value: &Commit<'_>) -> Self {
        let summary = value.summary().ok().flatten().unwrap_or("<no summary>");
        Self {
            summary: summary.to_string(),
            sha1: value.id().to_string(),
        }
    }
}
