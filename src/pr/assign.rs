use crate::config::{Config, ReleaseConfig};
use crate::error::{Error, Result};
use crate::package::Package;
use crate::pr::git::{changelog_range, is_notch_commit, parse_bump_trailer, remote_credentials};
use crate::pr::run::UpdatedCrate;
use crate::pr::traits::{CommitInfo, PackageCommits};
use anyhow::Context;
use cargo_metadata::semver::Version;
use git2::{Commit, DiffOptions, FetchOptions, Oid, Repository, Sort};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tracing::{debug, instrument};

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
    #[instrument(skip_all)]
    fn get(
        &mut self,
        config: &Config,
        packages: HashSet<Package>,
    ) -> Result<(HashMap<Package, Vec<CommitInfo>>, Repository, String)> {
        let repo = self
            .repo
            .take()
            .ok_or_else(|| Error::msg("WorktreeCommitAssigner's repo was already taken"))?;

        fetch_remote(&repo, config).context("fetch remote")?;
        let config = &config.release;

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

        debug!("Attributed: {attributed:?}");
        debug!("Changelog range: {changelog_range:?}");
        Ok((attributed, repo, changelog_range))
    }
}

// Updates the local `<remote>/<default_branch>` tracking ref before we diff against it. Without
// this, a stale local ref makes `commit_range()` include far more history than is actually
// unmerged, and every package ever touched in that stale range gets (incorrectly) flagged as changed.
fn fetch_remote(repo: &Repository, config: &Config) -> Result<()> {
    let release = &config.release;
    let mut remote = repo
        .find_remote(&release.remote)
        .context("get remote to fetch")?;

    let mut opts = FetchOptions::new();
    opts.remote_callbacks(remote_credentials(config.repo.token.clone()));

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

// Rebuilds an `UpdatedCrate` per bump that previous notch runs left in the upstream range, so a
// rerun's PR body can describe the whole branch rather than just the newest delta. The versions
// come from each bump commit's `Notch-Bump` trailer; the commits are the non-notch commits that
// landed since the previous bump (or the range start), attributed by path as usual. Trailer
// entries that don't parse, or name a crate that no longer exists, are skipped — those bumps
// just don't get a section, they don't fail the run.
pub fn prior_bump_sections(
    repo: &Repository,
    release: &ReleaseConfig,
    packages: &HashSet<Package>,
) -> Result<Vec<UpdatedCrate>> {
    let all_commits = get_commits(repo, release).context("get commits")?;

    let mut sections = Vec::new();
    let mut pending: Vec<Commit> = Vec::new();
    for commit in all_commits {
        if !is_notch_commit(&commit) {
            pending.push(commit);
            continue;
        }

        let bumped: Vec<(Package, Version)> = parse_bump_trailer(&commit)
            .into_iter()
            .filter_map(|record| {
                packages.iter().find(|p| p.name == record.name).map(|p| {
                    (
                        Package {
                            version: record.old,
                            ..p.clone()
                        },
                        record.new,
                    )
                })
            })
            .collect();

        let changed: HashSet<Package> = bumped.iter().map(|(p, _)| p.clone()).collect();
        let mut attributed = attribute_commits_to_packages(repo, &pending, changed)
            .context("attribute commits to prior bumps")?;

        for (package, new_version) in bumped {
            let commits = attributed.remove(&package).unwrap_or_default();
            sections.push(UpdatedCrate {
                package,
                new_version,
                commits,
            });
        }
        // even if the trailer yielded nothing, these commits belong to this bump — carrying them
        // into the next section would misattribute them
        pending.clear();
    }

    Ok(sections)
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
        // per the conventional commits spec, a breaking change can also be
        // declared in a footer rather than the header's `!` marker
        let breaking = value.message().is_ok_and(|message| {
            message
                .lines()
                .any(|l| l.starts_with("BREAKING CHANGE:") || l.starts_with("BREAKING-CHANGE:"))
        });
        Self {
            summary: summary.to_string(),
            sha1: value.id().to_string(),
            breaking,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cargo_metadata::semver::Version;
    use git2::Signature;
    use std::fs;

    fn init_repo(name: &str) -> Repository {
        let dir =
            std::env::temp_dir().join(format!("notch-assign-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Repository::init(&dir).unwrap()
    }

    fn sig() -> Signature<'static> {
        Signature::now("test", "test@example.com").unwrap()
    }

    // Writes `content` to `path` (relative to the repo's workdir, creating parent
    // directories as needed), stages it on top of whatever's already indexed from prior
    // calls, and commits with `parents`.
    fn commit_file(repo: &Repository, path: &str, content: &str, parents: &[&Commit]) -> Oid {
        let full_path = repo.workdir().unwrap().join(path);
        fs::create_dir_all(full_path.parent().unwrap()).unwrap();
        fs::write(&full_path, content).unwrap();

        let mut index = repo.index().unwrap();
        index.add_path(Path::new(path)).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();

        let sig = sig();
        repo.commit(Some("HEAD"), &sig, &sig, "test commit", &tree, parents)
            .unwrap()
    }

    fn package(path: &str) -> Package {
        Package {
            path: path.to_string(),
            name: path.replace('/', "-"),
            version: Version::new(0, 1, 0),
        }
    }

    fn cleanup(repo: &Repository) {
        let _ = fs::remove_dir_all(repo.workdir().unwrap());
    }

    #[test]
    fn only_packages_whose_path_changed_are_returned() {
        let repo = init_repo("changed-paths");
        let c0 = commit_file(&repo, "crate-a/Cargo.toml", "a", &[]);
        let c0_commit = repo.find_commit(c0).unwrap();
        let base = commit_file(&repo, "crate-b/Cargo.toml", "b", &[&c0_commit]);
        let base_commit = repo.find_commit(base).unwrap();
        commit_file(&repo, "crate-a/src/lib.rs", "fn a() {}", &[&base_commit]);

        // upstream ref points at `base`, before crate-a's src file was touched
        repo.reference("refs/remotes/origin/master", base, true, "test")
            .unwrap();

        let packages = HashSet::from([package("crate-a"), package("crate-b")]);
        let release = ReleaseConfig::default();

        let changed = get_changed_packages(&repo, &release, packages, None).unwrap();

        assert_eq!(changed, HashSet::from([package("crate-a")]));
        cleanup(&repo);
    }

    #[test]
    fn root_package_is_always_considered_changed() {
        let repo = init_repo("root-package");
        let base = commit_file(&repo, "unrelated.txt", "x", &[]);

        // HEAD == upstream, so the diff between them is empty
        repo.reference("refs/remotes/origin/master", base, true, "test")
            .unwrap();

        let packages = HashSet::from([package(".")]);
        let release = ReleaseConfig::default();

        let changed = get_changed_packages(&repo, &release, packages, None).unwrap();

        assert_eq!(changed, HashSet::from([package(".")]));
        cleanup(&repo);
    }

    #[test]
    fn base_override_bypasses_upstream_ref_resolution() {
        let repo = init_repo("base-override");
        let c0 = commit_file(&repo, "crate-a/Cargo.toml", "a", &[]);
        let marker_commit = repo.find_commit(c0).unwrap();
        commit_file(&repo, "crate-a/src/lib.rs", "fn a() {}", &[&marker_commit]);

        // no `origin` remote/ref configured at all — if the code tried to resolve
        // `origin/master` here it would error out
        let packages = HashSet::from([package("crate-a")]);
        let release = ReleaseConfig::default();

        let changed =
            get_changed_packages(&repo, &release, packages, Some(&marker_commit)).unwrap();

        assert_eq!(changed, HashSet::from([package("crate-a")]));
        cleanup(&repo);
    }

    #[test]
    fn prior_bump_sections_recovers_bumps_from_trailers() {
        let repo = init_repo("prior-bumps");
        let base = commit_file(&repo, "crate-a/Cargo.toml", "a", &[]);
        let base_commit = repo.find_commit(base).unwrap();
        repo.reference("refs/remotes/origin/master", base, true, "test")
            .unwrap();

        // a human commit touching crate-a, then the bump commit a prior run made for it
        let human = commit_file(&repo, "crate-a/src/lib.rs", "fn a() {}", &[&base_commit]);
        let human_commit = repo.find_commit(human).unwrap();

        let sig = crate::pr::git::notch_signature().unwrap();
        let tree = human_commit.tree().unwrap();
        let notch_oid = repo
            .commit(
                Some("HEAD"),
                &sig,
                &sig,
                "chore(notch): changelog + version bump\n\nNotch-Bump: crate-a@0.1.0->0.2.0",
                &tree,
                &[&human_commit],
            )
            .unwrap();
        let notch_commit = repo.find_commit(notch_oid).unwrap();

        // a newer human commit after the bump belongs to the *next* run, not a prior section
        commit_file(&repo, "crate-a/src/more.rs", "fn b() {}", &[&notch_commit]);

        let packages = HashSet::from([package("crate-a")]);
        let release = ReleaseConfig::default();

        let sections = prior_bump_sections(&repo, &release, &packages).unwrap();

        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].package.name, "crate-a");
        assert_eq!(sections[0].package.version, Version::new(0, 1, 0));
        assert_eq!(sections[0].new_version, Version::new(0, 2, 0));
        assert_eq!(sections[0].commits.len(), 1);
        assert_eq!(sections[0].commits[0].sha1, human.to_string());
        cleanup(&repo);
    }

    #[test]
    fn commits_are_attributed_to_the_package_whose_path_they_touch() {
        let repo = init_repo("attribute-basic");
        let c0 = commit_file(&repo, "crate-a/Cargo.toml", "a", &[]);
        let c0_commit = repo.find_commit(c0).unwrap();
        let c1 = commit_file(&repo, "crate-b/Cargo.toml", "b", &[&c0_commit]);
        let c1_commit = repo.find_commit(c1).unwrap();
        let c2 = commit_file(&repo, "crate-a/src/lib.rs", "fn a() {}", &[&c1_commit]);
        let c2_commit = repo.find_commit(c2).unwrap();
        let c3 = commit_file(&repo, "crate-b/src/lib.rs", "fn b() {}", &[&c2_commit]);

        let commits = vec![repo.find_commit(c2).unwrap(), repo.find_commit(c3).unwrap()];
        let changed = HashSet::from([package("crate-a"), package("crate-b")]);

        let attributed = attribute_commits_to_packages(&repo, &commits, changed).unwrap();

        assert_eq!(attributed[&package("crate-a")].len(), 1);
        assert_eq!(attributed[&package("crate-b")].len(), 1);
        cleanup(&repo);
    }

    #[test]
    fn merge_commits_are_excluded_from_attribution() {
        let repo = init_repo("attribute-merge");
        let base = commit_file(&repo, "crate-a/Cargo.toml", "a", &[]);
        let base_commit = repo.find_commit(base).unwrap();
        let side = commit_file(&repo, "crate-a/side.rs", "side", &[&base_commit]);
        let side_commit = repo.find_commit(side).unwrap();

        // A merge commit (2 parents) whose tree also touches crate-a — should still be
        // excluded, since a merge's diff against a single parent isn't a faithful
        // account of what the merge itself changed.
        let merge_sig = sig();
        let merge_tree = side_commit.tree().unwrap();
        let merge_oid = repo
            .commit(
                Some("HEAD"),
                &merge_sig,
                &merge_sig,
                "merge",
                &merge_tree,
                &[&side_commit, &base_commit],
            )
            .unwrap();

        let commits = vec![
            repo.find_commit(side).unwrap(),
            repo.find_commit(merge_oid).unwrap(),
        ];
        let changed = HashSet::from([package("crate-a")]);

        let attributed = attribute_commits_to_packages(&repo, &commits, changed).unwrap();

        assert_eq!(attributed[&package("crate-a")].len(), 1);
        cleanup(&repo);
    }

    #[test]
    fn root_package_gets_every_non_merge_commit() {
        let repo = init_repo("attribute-root");
        let c0 = commit_file(&repo, "unrelated-a.txt", "a", &[]);
        let c0_commit = repo.find_commit(c0).unwrap();
        let c1 = commit_file(&repo, "unrelated-b.txt", "b", &[&c0_commit]);
        let c1_commit = repo.find_commit(c1).unwrap();
        let c2 = commit_file(&repo, "unrelated-c.txt", "c", &[&c1_commit]);

        let commits = vec![repo.find_commit(c1).unwrap(), repo.find_commit(c2).unwrap()];
        let changed = HashSet::from([package(".")]);

        let attributed = attribute_commits_to_packages(&repo, &commits, changed).unwrap();

        assert_eq!(attributed[&package(".")].len(), 2);
        cleanup(&repo);
    }
}
