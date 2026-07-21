use crate::cmd::run_command;
use crate::config;
use crate::error::{Error, Result};
use crate::package::Package;
use crate::pr::assign::{WorktreeCommitAssigner, prior_bump_sections};
use crate::pr::git::{commit_changes, open_pr, push_current_branch};
use crate::pr::packages::CargoPackager;
use crate::pr::traits::{CommitInfo, PackageCommits, Packages};
use crate::pr::tui;
use anyhow::Context;
use cargo_metadata::semver::Version;
use git2::Repository;
use secrecy::ExposeSecret;
use tracing::{debug, info};

pub fn run() -> Result<()> {
    // load the config
    let config = config::load().context("load notch.toml")?;

    // validate we have a github PAT to work with
    // NOTE: This is better than requiring the token as a cli arg because we can pass it as an env
    // override, which avoids poluting your shell history.
    let Some(token) = config.repo.token.clone() else {
        return Err(Error::msg("No token provided"));
    };

    // get our packages
    let cargo_packages = CargoPackager::new(".".to_string());
    let packages = cargo_packages.get().context("get packages")?;
    debug!("packages: {packages:?}");

    // keep a copy: rebuilding the PR body later needs to map crate names from prior bump
    // trailers back to their packages
    let all_packages = packages.clone();

    // get commits for each package
    let repo: Repository = Repository::init(".").context("open repo")?;
    let mut worktree_assigner = WorktreeCommitAssigner::new(repo);
    let (changed_packages_with_commits, repo, changelog_range) = worktree_assigner
        .get(&config.release, packages)
        .context("get commits for packages")?;

    // nothing to do, just return
    if changed_packages_with_commits.is_empty() {
        println!("No packages to update, not creating commits or a release pr");
        return Ok(());
    }

    // run the tui so users pick the commit bumps
    let Some(res) = tui::run(changed_packages_with_commits).context("select version bumps")? else {
        info!("Cancelled, no changes made");
        return Ok(());
    };

    // bumps that previous runs already committed on this branch, recovered from their trailers —
    // the PR body must describe the whole branch, not just this run's delta. Computed before
    // commit_changes so this run's own bump commit isn't in the walked range.
    let prior_bumps = prior_bump_sections(&repo, &config.release, &all_packages)
        .context("collect prior bump sections")?;

    // actually update the package
    for updated_package in &res {
        update_package(updated_package, &changelog_range).context("update the package")?;
    }

    // cargo generate-lockfile so we update everything we need
    generate_lockfile().context("generate new lockfile")?;

    // commit changes
    commit_changes(&repo, &res).context("commit changes to the repo")?;

    // push to the remote
    push_current_branch(&repo, &config.release).context("push current branch")?;

    // the PR describes every bump on the branch: prior runs' sections (oldest first), then this
    // run's
    let mut all_updated = prior_bumps;
    all_updated.extend(res);

    // open the PR
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("spawn runtime")?;

    rt.block_on(open_pr(&repo, &config, token.expose_secret(), &all_updated))
        .context("open PR on runtime")?;

    Ok(())
}

/// update the lockfile for the current project
#[inline]
pub fn generate_lockfile() -> Result<()> {
    run_command(&["cargo", "generate-lockfile"]).context("call cargo generate-lockfile")?;
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
    let s = s.replacen(
        &format!("version = \"{}\"", updated_crate.package.version),
        &format!("version = \"{}\"", updated_crate.new_version),
        1,
    );
    std::fs::write(original_location, s).context("write updated back")?;
    Ok(())
}

pub fn update_package(updated_crate: &UpdatedCrate, commit_range: &str) -> Result<()> {
    // create a backup of the current Cargo.toml
    let real_file = format!("{}/Cargo.toml", updated_crate.package.path);
    let tmp_file = format!("{}/Cargo.toml.bak", updated_crate.package.path);

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

    // generate the changelog entry using git cliff,
    generate_changelog(
        &updated_crate.new_version.to_string(),
        &updated_crate.package.path,
        commit_range,
    )
    .context("generate changelog")?;
    Ok(())
}
