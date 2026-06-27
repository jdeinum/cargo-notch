use anyhow::{Context, Error, Result};
use cargo_metadata::{MetadataCommand, semver::Version};
use git2::{
    Commit, Cred, DiffOptions, IndexAddOption, Oid, PushOptions, RemoteCallbacks, Repository, Sort,
};
use octocrab::Octocrab;
use std::{
    collections::{HashMap, HashSet},
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};
use tracing::{error, info};

fn main() {
    tracing_subscriber::fmt::init();
    match run() {
        Ok(()) => {}
        Err(e) => error!("Error running the build tool: {e:?}"),
    }
}

fn run() -> Result<()> {
    let cleaned_members = get_cleaned_members().context("get cleaned members")?;

    let repo: Repository = Repository::init(".").context("open repo")?;

    let commits = get_commits(&repo).context("get commits")?;

    let changed_crates = get_changed_crates_with_commits(&repo, &commits, &cleaned_members)
        .context("get changed crates")?;

    for ccrate in changed_crates {
        update_package(&ccrate.0).context("Update package")?;
    }

    // cargo generate-lockfile so we update everything we need
    generate_lockfile().context("generate new lockfile")?;

    commit_changes(&repo).context("commit changes to the repo")?;

    // push to the remote
    // requires we have access to the SSH agent on our system, not quite sure how to do that yet
    // push_current_branch(&repo).context("push current branch")?;

    // open PR to origin master with auto merge and stuff
    // open_pr("jdeinum", "repo", "a branch", "master", "a token")
    // don't think I can do this with git2, need to do this with octocrab

    Ok(())
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct Crate {
    name: String,
    version: MyVersion,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct MyVersion {
    version: Version,
}

impl MyVersion {
    fn bump_patch(&self) -> Version {
        let mut new = self.version.clone();
        new.patch += 1;
        new
    }

    fn bump_minor(&self) -> Version {
        let mut new = self.version.clone();
        new.minor += 1;
        new
    }

    fn bump_major(&self) -> Version {
        let mut new = self.version.clone();
        new.major += 1;
        new
    }
}

fn get_cleaned_members() -> Result<Vec<Crate>> {
    // get the list of crates in this worktree
    let metadata = MetadataCommand::new().exec().unwrap();
    let members = metadata.workspace_members;
    let packages = metadata.packages;
    info!("Members: {members:?}");

    let pwd: PathBuf = std::env::current_dir().context("get current dir")?;
    info!("current dir: {pwd:?}");

    // clean up the members
    let cleaned_members: Vec<Crate> = members
        .iter()
        .map(|s| {
            let x: String = s
                .repr
                .replace("path+file://", "")
                .replace(&format!("{}/", pwd.to_str().unwrap()), "")
                .split('#')
                .next()
                .unwrap()
                .to_string();

            let v = packages
                .iter()
                .find(|p| p.id == *s)
                .unwrap()
                .version
                .clone();
            Crate {
                name: x,
                version: MyVersion { version: v },
            }
        })
        .collect();

    info!("cleaned members: {cleaned_members:?}");
    Ok(cleaned_members)
}

fn get_commits(repo: &Repository) -> Result<Vec<Commit<'_>>> {
    // find the list of commits present locally but not origin/master
    let mut revwalk = repo.revwalk().context("create revwalk")?;

    // we want the last commit present on origin master so we can find what changed in our first commit
    let commit_range = "origin/master..HEAD".to_string();
    revwalk
        .push_range(&commit_range)
        .context("revwalk commit range")?;
    revwalk.set_sorting(Sort::TOPOLOGICAL | Sort::REVERSE)?;
    let x: Result<Vec<Oid>, git2::Error> = revwalk.collect();
    let x = x.context("get oids")?;
    let commits: Result<Vec<Commit>, git2::Error> =
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

        for file in files {
            for cratem in crates {
                if file.starts_with(&cratem.name) {
                    changed_crates
                        .entry(cratem.clone())
                        .or_default()
                        .push(commit.clone());
                }
            }
        }
    }

    for (c_crate, commits) in &changed_crates {
        info!("Crate {} changed from the following commits:", c_crate.name);
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

fn generate_changelog(tag: &str, crate_name: &str) -> Result<()> {
    let res = Command::new("git")
        .args([
            "cliff",
            "--tag",
            tag,
            "--prepend",
            &format!("{crate_name}/CHANGELOG.md"),
            "origin/master..HEAD",
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

fn update_package(ccrate: &Crate) -> Result<()> {
    // suggest a list of version bumps for each service changed
    let cur_ver = &ccrate.version;
    let options: (Version, Version, Version) = (
        cur_ver.bump_patch(),
        cur_ver.bump_minor(),
        cur_ver.bump_major(),
    );

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
    let real_file = format!("{}/Cargo.toml", ccrate.name);
    let tmp_file = format!("{}/Cargo.toml.bak", ccrate.name);

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
    generate_changelog(&selected.to_string(), &ccrate.name).context("generate changelog")?;

    Ok(())
}

fn push_current_branch(repo: &Repository) -> Result<()> {
    let head = repo.head().context("get head")?;
    let branch = head.name().context("get head name")?;

    let mut remote = repo
        .find_remote("origin")
        .context("get remote for origin")?;

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

async fn open_pr(
    username: &str,
    repo: &str,
    source_branch: &str,
    target_branch: &str,
    token: &str,
) -> Result<()> {
    let octocrab = Octocrab::builder()
        .personal_token(token)
        .build()
        .context("build octocrab")?;

    let pr = octocrab
        .pulls(username, repo)
        .create("Chore: merging", source_branch, target_branch)
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
