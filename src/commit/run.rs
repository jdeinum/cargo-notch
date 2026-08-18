use crate::commit::auto;
use crate::commit::fsm::NotchFsm;
use crate::commit::tui;
use crate::config::{self, Config};
use crate::error::{Error, Result};
use crate::utils::command::run_command;
use crate::utils::commits::{CommitInfo, PackageCommits, WorktreeCommitAssigner, fetch_remote};
use crate::utils::git::{
    commit_changes, drop_prior_notch_commit, packages_without_prior_notch_commit,
};
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
/// and off which commits. Computing it is entirely read-only — no file written, no ref moved —
/// which is what makes `--dry-run` nothing more than "stop here and print it", see `describe`.
pub struct ReleasePlan {
    pub repo: Repository,
    pub updates: Vec<UpdatedCrate>,
    /// The commit range git-cliff should scan to build the changelog.
    pub changelog_range: String,
}

/// `notch commit`: bumps each changed package's version, updates its changelog, and commits the
/// result locally — no push, no PR (that's `notch pr`, which runs this same work via `commit`
/// before pushing and opening the PR). Doesn't require a github token, unlike `notch pr`, since
/// it never touches the remote.
pub fn run(auto: bool, dry_run: bool) -> Result<()> {
    let config = config::load().context("load notch.toml")?;

    let Some(plan) = plan(&config, auto).context("plan the release")? else {
        return Ok(());
    };

    // `--dry-run` is exactly "don't run the half that writes". `plan` left the repo as it found
    // it, so there's nothing to undo and nothing to caveat in what `describe` prints.
    if dry_run {
        describe(&plan);
        return Ok(());
    }

    apply(&config, plan).context("apply the release")?;
    Ok(())
}

/// Decides what to release, without writing any of it or moving any ref:
///
/// 1. reads each package's baseline version — what it would be without the branch's own prior
///    notch bump, which is worked out in a throwaway worktree so this branch is never touched
///    (see `utils::git::packages_without_prior_notch_commit`)
/// 2. finds the packages in the project and, per package, the commits attributed to it
/// 3. builds one state machine per changed package, fed with its attributed commits — its
///    suggested bump (see `NotchFsm::dump`) is what auto mode accepts and what the tui preselects
/// 4. picks each package's bump — accepted as-is with `auto`, otherwise confirmed or overridden
///    interactively via the tui
///
/// Returns `None` if there's nothing to release: nothing changed, every commit matched the skip
/// list, or the user cancelled the tui.
pub fn plan(config: &Config, auto: bool) -> Result<Option<ReleasePlan>> {
    let repo: Repository = Repository::init(".").context("open repo")?;

    // serialize against any other notch process touching this repo before we read any of its git
    // state — see `acquire_repo_lock`. Read-only work takes the lock too: a concurrent real run
    // moving that state underneath us would make the plan a lie.
    acquire_repo_lock(&repo).context("acquire notch repo lock")?;

    // the tracking ref every commit range below resolves against has to be current — see
    // `fetch_remote`
    fetch_remote(&repo, config).context("fetch remote")?;

    // everything below runs against the repo root, which is where notch is invoked from
    let root = Path::new(".");
    let ecosystem = CargoEcosystem;

    // Each package's baseline version: what's on disk, unless a prior notch commit already bumped
    // it, in which case what it would be without that commit. Working that out needs the commit
    // rebased away, which happens in a throwaway worktree — planning decides, `apply` rewrites.
    let mut packages = match packages_without_prior_notch_commit(&repo, config, &ecosystem)
        .context("read baseline packages")?
    {
        Some(packages) => packages,
        None => ecosystem.packages(root).context("get packages")?,
    };

    // The ecosystem has already subtracted any nested packages; this layers the user's own
    // `[tracking]` excludes on top, so both are in place before anything asks a package whether a
    // file belongs to it.
    narrow_to_tracked(&mut packages, &config.tracking);

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
    }))
}

/// Writes the plan out: every package's new version, its changelog entry, a refreshed lockfile,
/// and one commit holding the lot. Everything in this function mutates something; everything that
/// merely decides happens in `plan`.
pub fn apply(config: &Config, plan: ReleasePlan) -> Result<(Repository, Vec<UpdatedCrate>)> {
    let ReleasePlan {
        repo,
        updates,
        changelog_range,
    } = plan;

    // Now, and only now, the branch gets rewritten. `plan` deliberately left the prior notch
    // commit alone; dropping it here is what keeps the branch at exactly one notch commit, and it
    // also restores the manifests to the versions `updates` was computed against — which is what
    // `set_versions` checks before it writes anything.
    drop_prior_notch_commit(&repo, config).context("drop prior notch commit")?;

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
