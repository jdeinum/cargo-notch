use crate::commit::auto;
use crate::commit::fsm::NotchFsm;
use crate::commit::tui;
use crate::config::{self, Config};
use crate::error::{Error, Result};
use crate::utils::command::run_command;
use crate::utils::commits::{CommitInfo, PackageCommits, WorktreeCommitAssigner};
use crate::utils::generate_lockfile;
use crate::utils::git::{commit_changes, drop_prior_notch_commit};
use crate::utils::lock::acquire_repo_lock;
use crate::utils::package::Package;
use crate::utils::packages::{CargoPackager, Packages};
use anyhow::Context;
use cargo_metadata::semver::Version;
use git2::{Repository, Status};
use std::collections::HashMap;
use tracing::info;

/// `notch commit`: bumps each changed package's version, updates its changelog, and commits the
/// result locally — no push, no PR (that's `notch pr`, which runs this same work via `commit`
/// before pushing and opening the PR). Doesn't require a github token, unlike `notch pr`, since
/// it never touches the remote.
pub fn run(auto: bool) -> Result<()> {
    let config = config::load().context("load notch.toml")?;
    commit(&config, auto)?;
    Ok(())
}

/// Bumps each changed package's version, updates its changelog, and commits the result locally —
/// no push, no PR. Shared by `notch commit` (which stops here, see `run`) and `notch pr` (which
/// pushes and opens a PR on top, see `pr::run`):
///
/// 1. drops the branch's prior notch commit, if one exists (see `utils::git::drop_prior_notch_commit`),
///    so every step below sees a clean slate rather than having to route around a stale one
/// 2. finds the packages in the project and, per package, the commits attributed to it
/// 3. builds one state machine per changed package, fed with its attributed commits — its
///    suggested bump (see `NotchFsm::dump`) is what auto mode accepts and what the tui preselects
/// 4. picks each package's bump — accepted as-is with `auto`, otherwise confirmed or overridden
///    interactively via the tui — then updates each package and commits
///
/// Returns `None` if there's nothing to release: nothing changed, every commit matched the skip
/// list, or the user cancelled the tui.
pub fn commit(config: &Config, auto: bool) -> Result<Option<(Repository, Vec<UpdatedCrate>)>> {
    let repo: Repository = Repository::init(".").context("open repo")?;

    // serialize against any other notch process touching this repo before we read or mutate
    // any of its git state — see `acquire_repo_lock`
    acquire_repo_lock(&repo).context("acquire notch repo lock")?;

    // drop any prior notch commit already on this branch before doing anything else — this
    // checks the working tree back out to its pre-bump state, so package discovery below MUST
    // run after it, not before: reading package versions off a working tree that still has a
    // prior notch commit's bump applied is exactly how you get a wrong "current version" and a
    // silently-wrong bump on top of it
    drop_prior_notch_commit(&repo, config).context("drop prior notch commit")?;

    // get our packages — after the drop above, so this sees each package's true, un-bumped
    // version rather than whatever a prior (now-dropped) notch commit had left on disk
    let cargo_packages = CargoPackager::new(".".to_string());
    let packages = cargo_packages.get().context("get packages")?;

    // get commits for each package
    let mut worktree_assigner = WorktreeCommitAssigner::new(repo);
    let (changed_packages_with_commits, repo, changelog_range) = worktree_assigner
        .get(config, packages)
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
    let res = if auto {
        let res = auto::select(fsms, &config.bumps);
        // every changed package's commits can match the skip list, in which
        // case there's nothing left to release
        if res.is_empty() {
            println!("No packages to update, not creating commits or a release pr");
            return Ok(None);
        }
        res
    } else {
        let Some(res) = tui::run(fsms).context("select version bumps")? else {
            info!("Cancelled, no changes made");
            return Ok(None);
        };
        res
    };

    // actually update each package: changelog entry + Cargo.toml version bump
    for updated_package in &res {
        update_package(&repo, updated_package, &changelog_range).context("update the package")?;
    }

    // cargo generate-lockfile so we update everything we need
    generate_lockfile().context("generate new lockfile")?;

    // commit changes
    commit_changes(&repo, &res).context("commit changes to the repo")?;

    Ok(Some((repo, res)))
}

pub struct UpdatedCrate {
    pub package: Package,
    pub new_version: Version,
    pub commits: Vec<CommitInfo>,
}

#[inline]
pub fn backup_config(original_location: &str, backup_location: &str) -> Result<()> {
    run_command(&["cp", original_location, backup_location]).context("create cargo.toml backup")?;
    Ok(())
}

/// Restore the original cargo.toml
#[inline]
pub fn restore_backup_config(original_location: &str, backup_location: &str) -> Result<()> {
    run_command(&["mv", backup_location, original_location])
        .context("restore backup cargo.toml")?;
    Ok(())
}

#[inline]
pub fn update_package_version(updated_crate: &UpdatedCrate, original_location: &str) -> Result<()> {
    // update the current one in place, with the bumped version
    // cowboying this for now
    // TODO: Use a proper crate for updating this
    // WARN: If you have a dep with this version before the package version, ur cooked
    let s = std::fs::read_to_string(original_location).context("read Cargo.toml")?;

    let needle = format!("version = \"{}\"", updated_crate.package.version);
    if !s.contains(&needle) {
        return Err(Error::msg(format!(
            "expected to find `{needle}` in {original_location}, but it wasn't there — \
             `{}` is out of sync with what's actually on disk",
            updated_crate.package.version
        )));
    }

    let s = s.replacen(
        &needle,
        &format!("version = \"{}\"", updated_crate.new_version),
        1,
    );
    std::fs::write(original_location, s).context("write updated back")?;
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

pub fn update_package(
    repo: &Repository,
    updated_crate: &UpdatedCrate,
    commit_range: &str,
) -> Result<()> {
    // create a backup of the current Cargo.toml
    let real_file = updated_crate.package.join("Cargo.toml");
    let tmp_file = updated_crate.package.join("Cargo.toml.bak");

    // check if either the changelog or the Cargo.toml has changes not yet commited, if so, we
    // return an error. We don't update anything for consistency, its an all or nothing
    let changelog_path = updated_crate.package.join("CHANGELOG.md");

    if is_file_dirty(repo, real_file.as_ref()).context("check if Cargo.toml dirty")? {
        return Err(Error::msg(
            "Cargo.toml is dirty, please commit the changes and try again",
        ));
    }

    if is_file_dirty(repo, changelog_path.as_ref()).context("check if Cargo.toml dirty")? {
        return Err(Error::msg(
            "Cargo.toml is dirty, please commit the changes and try again",
        ));
    }

    // create a backup
    backup_config(&real_file, &tmp_file).context("backup config")?;

    // update the package version in cargo.toml
    match update_package_version(updated_crate, &real_file) {
        Ok(()) => {
            // delete the temp file
            std::fs::remove_file(tmp_file).context("delete temp file")?;
        }
        Err(e) => {
            // move the backup to the OG location
            restore_backup_config(&real_file, &tmp_file).context("restore backup cargo.toml")?;
            return Err(e);
        }
    }

    // git cliff --prepend requires the changelog to exist — a crate that has never released
    // before won't have one yet
    if !std::path::Path::new(&changelog_path).exists() {
        std::fs::write(&changelog_path, "").context("create empty changelog")?;
    }

    // generate the changelog entry using git cliff,
    generate_changelog(
        &updated_crate.new_version.to_string(),
        &updated_crate.package.path,
        commit_range,
    )
    .context("generate changelog")?;
    Ok(())
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
