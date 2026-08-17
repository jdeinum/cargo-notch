use crate::commit::auto;
use crate::commit::fsm::NotchFsm;
use crate::commit::tui;
use crate::config::{self, Config};
use crate::error::{Error, Result};
use crate::utils::command::run_command;
use crate::utils::commits::{CommitInfo, PackageCommits, WorktreeCommitAssigner, fetch_remote};
use crate::utils::git::{commit_changes, drop_prior_notch_commit, prior_notch_state};
use crate::utils::lock::acquire_repo_lock;
use crate::utils::package::Package;
use crate::utils::packages::{CargoEcosystem, Ecosystem, narrow_to_tracked};
use anyhow::Context;
use cargo_metadata::semver::Version;
use git2::{Repository, Status};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::info;

/// Everything a release run decided before it wrote anything: which packages to bump, to what,
/// and off which commits. Computing this is entirely read-only apart from the prior-notch-commit
/// drop, which is why `--dry-run` can stop here and print it — see `describe`.
pub struct ReleasePlan {
    pub repo: Repository,
    pub updates: Vec<UpdatedCrate>,
    /// The commit range git-cliff should scan to build the changelog.
    pub changelog_range: String,
    /// Short id of a prior notch commit this run deliberately left on the branch. Only ever set
    /// for a dry run — a real run drops it, so there's nothing left to report.
    pub prior_notch_commit: Option<String>,
}

/// `notch commit`: bumps each changed package's version, updates its changelog, and commits the
/// result locally — no push, no PR (that's `notch pr`, which runs this same work via `commit`
/// before pushing and opening the PR). Doesn't require a github token, unlike `notch pr`, since
/// it never touches the remote.
pub fn run(auto: bool, dry_run: bool) -> Result<()> {
    let config = config::load().context("load notch.toml")?;

    if dry_run {
        if let Some(plan) = plan(&config, auto, true).context("plan the release")? {
            describe(&plan);
        }
        return Ok(());
    }

    commit(&config, auto)?;
    Ok(())
}

/// Bumps each changed package's version, updates its changelog, and commits the result locally —
/// no push, no PR. Shared by `notch commit` (which stops here, see `run`) and `notch pr` (which
/// pushes and opens a PR on top, see `pr::run`). Just `plan` then `apply`; the split exists so
/// `--dry-run` can run the first half, which decides everything, without the second, which is
/// the half that writes.
pub fn commit(config: &Config, auto: bool) -> Result<Option<(Repository, Vec<UpdatedCrate>)>> {
    let Some(plan) = plan(config, auto, false)? else {
        return Ok(None);
    };
    apply(plan).map(Some)
}

/// Decides what to release, without writing any of it:
///
/// 1. drops the branch's prior notch commit, if one exists (see `utils::git::drop_prior_notch_commit`),
///    so every step below sees a clean slate rather than having to route around a stale one — or,
///    under `dry_run`, recovers the same baseline from that commit's trailer instead of dropping it
/// 2. finds the packages in the project and, per package, the commits attributed to it
/// 3. builds one state machine per changed package, fed with its attributed commits — its
///    suggested bump (see `NotchFsm::dump`) is what auto mode accepts and what the tui preselects
/// 4. picks each package's bump — accepted as-is with `auto`, otherwise confirmed or overridden
///    interactively via the tui
///
/// Returns `None` if there's nothing to release: nothing changed, every commit matched the skip
/// list, or the user cancelled the tui.
pub fn plan(config: &Config, auto: bool, dry_run: bool) -> Result<Option<ReleasePlan>> {
    let repo: Repository = Repository::init(".").context("open repo")?;

    // serialize against any other notch process touching this repo before we read or mutate
    // any of its git state — see `acquire_repo_lock`. A dry run takes the lock too: it reads the
    // same state, and a concurrent real run moving it underneath would make the plan a lie.
    acquire_repo_lock(&repo).context("acquire notch repo lock")?;

    // A prior notch commit's bump is sitting in the working tree, so package discovery below
    // would read it as the package's "current version" and bump on top of it. A real run fixes
    // that by dropping the commit before discovery — but that rewrites history, which a dry run
    // must not do. The same pre-bump versions are already recorded in the commit's own
    // `Notch-Bump` trailer, so a dry run recovers the baseline from there and leaves the branch
    // exactly as it found it.
    let prior = if dry_run {
        // the tracking ref the commit range is resolved against has to be current either way
        fetch_remote(&repo, config).context("fetch remote")?;
        prior_notch_state(&repo, config).context("read prior notch commit")?
    } else {
        drop_prior_notch_commit(&repo, config).context("drop prior notch commit")?;
        None
    };

    // everything below runs against the repo root, which is where notch is invoked from
    let root = Path::new(".");
    let ecosystem = CargoEcosystem;

    // get our packages — after the drop above, so this sees each package's true, un-bumped
    // version rather than whatever a prior (now-dropped) notch commit had left on disk
    let mut packages = ecosystem.packages(root).context("get packages")?;

    // The ecosystem has already subtracted any nested packages; this layers the user's own
    // `[tracking]` excludes on top, so both are in place before anything asks a package whether a
    // file belongs to it.
    narrow_to_tracked(&mut packages, &config.tracking);

    if let Some(prior) = &prior {
        for package in &mut packages {
            if let Some(baseline) = prior.baseline.get(&package.name) {
                package.version = baseline.clone();
            }
        }
    }

    // get commits for each package
    let mut worktree_assigner = WorktreeCommitAssigner::new(repo);
    let (changed_packages_with_commits, repo, changelog_range) = worktree_assigner
        .get(config, packages.into_iter().collect())
        .context("get commits for packages")?;

    // nothing to do, just return
    if changed_packages_with_commits.is_empty() {
        println!("No packages to update, not creating commits or a release pr");
        return Ok(None);
    }

    // one state machine per changed package, fed with its attributed commits
    let fsms: HashMap<Package, NotchFsm> = changed_packages_with_commits
        .into_iter()
        .map(|(package, commits)| {
            let mut fsm = NotchFsm::new(config.bumps.clone());
            fsm.handle_commits(&commits);
            (package, fsm)
        })
        .collect();

    // pick each package's bump: accept the state machine's suggestion with --auto,
    // otherwise confirm or override it interactively via the tui
    let updates = if auto {
        let updates = auto::select(fsms, &config.bumps);
        // every changed package's commits can match the skip list, in which
        // case there's nothing left to release
        if updates.is_empty() {
            println!("No packages to update, not creating commits or a release pr");
            return Ok(None);
        }
        updates
    } else {
        let Some(updates) = tui::run(fsms).context("select version bumps")? else {
            info!("Cancelled, no changes made");
            return Ok(None);
        };
        updates
    };

    Ok(Some(ReleasePlan {
        repo,
        updates,
        changelog_range,
        prior_notch_commit: prior.map(|p| p.commit),
    }))
}

/// Writes the plan out: every package's new version, its changelog entry, a refreshed lockfile,
/// and one commit holding the lot.
pub fn apply(plan: ReleasePlan) -> Result<(Repository, Vec<UpdatedCrate>)> {
    let ReleasePlan {
        repo,
        updates,
        changelog_range,
        ..
    } = plan;

    let root = Path::new(".");
    let ecosystem = CargoEcosystem;

    // check every package before writing any of them — a single dirty manifest or changelog
    // aborts the run with the working tree untouched, rather than partway through
    for update in &updates {
        ensure_writable(&repo, update).context("check the package can be updated")?;
    }

    // bump every manifest in one batch, so a package whose manifest has drifted out of sync
    // can't leave the packages ahead of it in the list already written — see `set_versions`
    let bumps: Vec<(Package, Version)> = updates
        .iter()
        .map(|c| (c.package.clone(), c.new_version.clone()))
        .collect();
    let mut touched = ecosystem
        .set_versions(root, &bumps)
        .context("update package versions")?;

    // changelog entry per package, prepended by git cliff
    for update in &updates {
        touched.push(update_changelog(update, &changelog_range).context("update the changelog")?);
    }

    // refresh the lockfile so it reflects the versions we just wrote
    touched.extend(ecosystem.refresh_lock(root).context("refresh lockfile")?);

    // commit changes, staging exactly the files the steps above reported writing
    commit_changes(&repo, &updates, &touched).context("commit changes to the repo")?;

    Ok((repo, updates))
}

/// Prints what `apply` would have written, for `--dry-run`.
pub fn describe(plan: &ReleasePlan) {
    println!("dry run — nothing written, nothing committed\n");

    for update in &plan.updates {
        println!(
            "  {} {} -> {}",
            update.package.name, update.package.version, update.new_version
        );
        for commit in &update.commits {
            println!("    {} {}", commit.short_id(), commit.summary);
        }
        println!(
            "    would write {} and {}",
            update.package.manifest.display(),
            update.package.join("CHANGELOG.md")
        );
        println!();
    }

    if let Some(commit) = &plan.prior_notch_commit {
        println!(
            "a prior notch commit ({commit}) is still on this branch. A real run drops it first; \
             this one left it alone and took each package's pre-bump version from its \
             `Notch-Bump` trailer, so the versions above are what a real run would produce."
        );
    }
}

pub struct UpdatedCrate {
    pub package: Package,
    pub new_version: Version,
    pub commits: Vec<CommitInfo>,
}

/// Refuses to touch a package whose manifest or changelog already has uncommitted changes. Run
/// for every package before any of them is written, so a single dirty file aborts the run with
/// the working tree exactly as the user left it — this is all or nothing.
fn ensure_writable(repo: &Repository, updated_crate: &UpdatedCrate) -> Result<()> {
    let changelog = PathBuf::from(updated_crate.package.join("CHANGELOG.md"));

    for path in [&updated_crate.package.manifest, &changelog] {
        if is_file_dirty(repo, path)
            .with_context(|| format!("check if {} dirty", path.display()))?
        {
            return Err(Error::msg(format!(
                "{} is dirty, please commit the changes and try again",
                path.display()
            )));
        }
    }

    Ok(())
}

#[inline]
fn is_file_dirty(repo: &Repository, path: &std::path::Path) -> Result<bool> {
    let status = match repo.status_file(path) {
        Ok(status) => status,
        // a file that doesn't exist at all yet (e.g. a crate's first CHANGELOG.md) can't be dirty
        Err(e) if e.code() == git2::ErrorCode::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };

    // "Dirty" = any change in the index or working tree
    Ok(status.intersects(
        Status::INDEX_NEW
            | Status::INDEX_MODIFIED
            | Status::INDEX_DELETED
            | Status::INDEX_RENAMED
            | Status::INDEX_TYPECHANGE
            | Status::WT_NEW
            | Status::WT_MODIFIED
            | Status::WT_DELETED
            | Status::WT_RENAMED
            | Status::WT_TYPECHANGE,
    ))
}

/// Prepends this release's entry to the package's changelog, returning the path it wrote so the
/// caller can stage it.
fn update_changelog(updated_crate: &UpdatedCrate, commit_range: &str) -> Result<PathBuf> {
    let changelog_path = updated_crate.package.join("CHANGELOG.md");

    // git cliff --prepend requires the changelog to exist — a crate that has never released
    // before won't have one yet
    if !std::path::Path::new(&changelog_path).exists() {
        std::fs::write(&changelog_path, "").context("create empty changelog")?;
    }

    generate_changelog(
        &updated_crate.new_version.to_string(),
        &updated_crate.package.path,
        commit_range,
    )
    .context("generate changelog")?;

    Ok(PathBuf::from(changelog_path))
}

/// Generate the changelog for the provided commit range
#[inline]
fn generate_changelog(tag: &str, crate_path: &str, commit_range: &str) -> Result<()> {
    run_command(&[
        "git",
        "cliff",
        "--tag",
        tag,
        "--prepend",
        &format!("{crate_path}/CHANGELOG.md"),
        commit_range,
    ])
    .context("generate changelog using git cliff")?;
    Ok(())
}
