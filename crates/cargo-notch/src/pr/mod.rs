use crate::config::{self, ReleaseConfig};
use crate::error::{Error, Result};
use crate::workspace::{Crate, get_cleaned_members};
use anyhow::Context;
use cargo_metadata::semver::Version;
use git2::{
    Commit, Cred, DiffOptions, IndexAddOption, Oid, PushOptions, RemoteCallbacks, Repository, Sort,
};
use octocrab::Octocrab;
use std::{
    collections::{HashMap, HashSet},
    io::Write,
    path::Path,
    process::Command,
};
use tracing::info;

pub fn run(token: &str) -> Result<()> {
    let pwd = std::env::current_dir().context("get current dir")?;
    let cleaned_members = get_cleaned_members(&pwd).context("get cleaned members")?;
    let config = config::load(&pwd).context("load notch.toml")?;

    let repo: Repository = Repository::init(".").context("open repo")?;
    let (owner, repo_name) =
        config::resolve_owner_repo(&repo, &config.repo).context("resolve owner/repo")?;

    fetch_remote(&repo, &config.release).context("fetch remote")?;

    let commits = get_commits(&repo, &config.release).context("get commits")?;

    let changed_crates = get_changed_crates_with_commits(&repo, &commits, &cleaned_members)
        .context("get changed crates")?;

    for (crate_info, commits) in changed_crates {
        update_package(&crate_info, &commits, &config.release).context("Update package")?;
    }

    // cargo generate-lockfile so we update everything we need
    generate_lockfile().context("generate new lockfile")?;

    // commit changes
    commit_changes(&repo).context("commit changes to the repo")?;

    // push to the remote
    // requires we have access to the SSH agent on our system, not quite sure how to do that yet
    push_current_branch(&repo, &config.release).context("push current branch")?;

    // open the PR
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("spawn runtime")?;
    rt.block_on(open_pr(&owner, &repo_name, &repo, &config.release, token))
        .context("open PR on runtime")?;

    Ok(())
}

// Updates the local `<remote>/<default_branch>` tracking ref before we diff
// against it. Without this, a stale local ref makes `commit_range()` include
// far more history than is actually unmerged, and every crate ever touched
// in that stale range gets (incorrectly) flagged as changed.
fn fetch_remote(repo: &Repository, release: &ReleaseConfig) -> Result<()> {
    let mut remote = repo
        .find_remote(&release.remote)
        .context("get remote to fetch")?;

    let mut callbacks = RemoteCallbacks::new();
    callbacks.credentials(|_url, username, _allowed| {
        Cred::ssh_key_from_agent(username.unwrap_or("git"))
    });

    let mut opts = git2::FetchOptions::new();
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
    let x: std::result::Result<Vec<Oid>, git2::Error> = revwalk.collect();
    let x = x.context("get oids")?;
    let commits: std::result::Result<Vec<Commit>, git2::Error> =
        x.iter().map(|c| repo.find_commit(*c)).collect();
    let commits = commits.context("get commits from oids")?;

    info!("Commits: {commits:?}");
    Ok(commits)
}

fn get_changed_crates_with_commits<'a>(
    repo: &'a Repository,
    commits: &'a [Commit],
    crates: &[Crate],
) -> Result<HashMap<Crate, Vec<Commit<'a>>>> {
    // get the list of updated services for each commit
    // to do this, we'll walk each commit, and check which services changed
    let mut changed_crates: HashMap<Crate, Vec<Commit>> = HashMap::new();
    for commit in commits {
        // find the files changed for this commit
        // NOTE: We just handle a single parent for now
        let parent_commit = commit.parent(0).context("get first parent")?;

        // get diff between the commit and its parent
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

        info!("Changed files for {:?} is {files:?}", commit.id());

        // find the crates that we actually changed with these files by cross referencing them to
        // the workplace members
        // we do that by removing the path up to and including the current dir for the packages, to
        // match how git sees it.

        for cratem in crates {
            if files.iter().any(|f| f.starts_with(&cratem.path)) {
                changed_crates
                    .entry(cratem.clone())
                    .or_default()
                    .push(commit.clone());
            }
        }
    }

    for (c_crate, commits) in &changed_crates {
        info!("Crate {} changed from the following commits:", c_crate.path);
        for c in commits {
            info!(
                "{}",
                c.summary()
                    .context("get summary for commit")
                    .unwrap()
                    .unwrap()
            );
        }
    }

    Ok(changed_crates)
}

fn commit_changes(repo: &Repository) -> Result<()> {
    // commit the changelog and version bumps
    let mut index = repo.index().context("get index for repo")?;
    index
        .add_all(std::iter::once(&"."), IndexAddOption::DEFAULT, None)
        .context("add all files to the index")?;
    index.write().context("write index to disk")?;
    let sig = repo.signature().context("get stored user details")?;
    let tree = repo
        .find_tree(index.write_tree().context("write tree for index")?)
        .context("find tree")?;
    let parent = repo
        .head()
        .context("get head of branch")?
        .peel_to_commit()
        .context("convert ref commit")?;

    repo.commit(
        Some("HEAD"),
        &sig,
        &sig,
        "chore: changelog + version bump",
        &tree,
        &[&parent],
    )
    .context("create the commit")?;

    Ok(())
}

fn generate_lockfile() -> Result<()> {
    let res = Command::new("cargo")
        .args(["generate-lockfile"])
        .spawn()
        .context("spawn child to copy cargo.toml")?
        .wait()
        .context("wait for child")?;

    if !res.success() {
        return Err(Error::msg("cargo generate-lockfile did not succeed"));
    }
    Ok(())
}

fn generate_changelog(tag: &str, crate_path: &str, release: &ReleaseConfig) -> Result<()> {
    let res = Command::new("git")
        .args([
            "cliff",
            "--tag",
            tag,
            "--prepend",
            &format!("{crate_path}/CHANGELOG.md"),
            &release.commit_range(),
        ])
        .spawn()
        .context("spawn child to run git cliff")?
        .wait()
        .context("wait for child to run git cliff")?;

    if !res.success() {
        return Err(Error::msg("cargo generate-lockfile did not succeed"));
    }

    Ok(())
}

fn update_package(ccrate: &Crate, commits: &[Commit], release: &ReleaseConfig) -> Result<()> {
    // suggest a list of version bumps for each service changed
    let cur_ver = &ccrate.version;
    let options: (Version, Version, Version) = (
        cur_ver.bump_patch(),
        cur_ver.bump_minor(),
        cur_ver.bump_major(),
    );

    // show the commits responsible for flagging this crate, oldest first, so
    // the user can sanity-check the suggested bump before picking one
    println!("Commits that changed {}:", ccrate.name);
    for c in commits {
        let summary = c.summary().ok().flatten().unwrap_or("<no summary>");
        println!("  {} {}", &c.id().to_string()[..7], summary);
    }

    // allow the user to override the version bump for each service
    print!(
        "updating {}\nselect one: \n0) bump patch to {}\n1) bump minor to {}\n2) bump major to {}\n> ",
        ccrate.name, options.0, options.1, options.2
    );
    std::io::stdout().flush().context("flush stdout")?;
    let mut s: String = String::new();
    let _ = std::io::stdin()
        .read_line(&mut s)
        .context("read choice from user")?;
    let s = s.replace('\n', "");
    let s = s.parse::<usize>().context("parse selection")?;

    let selected = match s {
        0 => options.0,
        1 => options.1,
        2 => options.2,
        _ => return Err(Error::msg("Not a valid selection")),
    };
    info!("User selected: {}", selected);

    // create a backup of the current Cargo.toml
    let real_file = format!("{}/Cargo.toml", ccrate.path);
    let tmp_file = format!("{}/Cargo.toml.bak", ccrate.path);

    let res = Command::new("cp")
        .args([&real_file, &tmp_file])
        .spawn()
        .context("spawn child to copy cargo.toml")?
        .wait()
        .context("wait for child")?;

    if !res.success() {
        return Err(Error::msg("Copy cargo.toml did not succeed"));
    }

    // update the current one in place, with the bumped version
    // cowboying this for now
    // TODO: Use a proper crate for updating this
    // NOTE: If you have a dep with this version before the package version, ur cooked
    let s = std::fs::read_to_string(&real_file).context("read Cargo.toml")?;
    let s = s.replacen(
        &format!("version = \"{}\"", cur_ver.version),
        &format!("version = \"{selected}\""),
        1,
    );

    // write it back
    match std::fs::write(&real_file, s).context("write updated back") {
        Ok(()) => {
            // delete the temp file
            std::fs::remove_file(tmp_file).context("delete temp file")?;
        }
        Err(e) => {
            // move the backup to the OG location
            let _res = Command::new("mv")
                .args([&tmp_file, &real_file])
                .spawn()
                .context("spawn child to restore backup")?
                .wait()
                .context("wait for child")?;
            // TODO: Do we even check the status here?
            return Err(e);
        }
    }

    // generate the changelog entry using git cliff,
    generate_changelog(&selected.to_string(), &ccrate.path, release)
        .context("generate changelog")?;

    Ok(())
}

fn push_current_branch(repo: &Repository, release: &ReleaseConfig) -> Result<()> {
    let head = repo.head().context("get head")?;
    let branch = head.name().context("get head name")?;

    let mut remote = repo.find_remote(&release.remote).context("get remote")?;

    info!("Found remote {}", release.remote);

    let mut callbacks = RemoteCallbacks::new();
    callbacks.credentials(|_url, username, _allowed| {
        Cred::ssh_key_from_agent(username.unwrap_or("git"))
    });

    let mut opts = PushOptions::new();
    opts.remote_callbacks(callbacks);

    // refspec: local:remote
    let refspec = format!("{branch}:{branch}");
    remote.push(&[&refspec], Some(&mut opts))?;

    Ok(())
}

// Runs only on a current-thread tokio runtime (see run()), so the future is
// never sent across threads despite git2 types not being Send.
#[allow(clippy::future_not_send)]
async fn open_pr(
    owner: &str,
    repo_name: &str,
    repo: &Repository,
    release: &ReleaseConfig,
    token: &str,
) -> Result<()> {
    let head = repo.head().context("get branch head")?;
    let branch = git2::Branch::wrap(head);
    let name = branch
        .name()
        .context("get local name")?
        .ok_or_else(|| Error::msg("No branch name"))?;

    let upstream = branch.upstream().context("get upstream branch")?;
    let upstream_branch_name = upstream
        .name()
        .context("get branch name")?
        .ok_or_else(|| Error::msg("No branch name"))?;
    info!(
        "Creating PR from {upstream_branch_name} into {}",
        release.default_branch
    );

    let octocrab = Octocrab::builder()
        .personal_token(token)
        .build()
        .context("build octocrab")?;

    let pr = octocrab
        .pulls(owner, repo_name)
        .create("Chore: release", name, &release.default_branch)
        .body("A release!")
        .send()
        .await
        .context("create PR")?;

    println!(
        "Opened PR #{}: {}",
        pr.number.unwrap(),
        pr.html_url.unwrap()
    );
    Ok(())
}
