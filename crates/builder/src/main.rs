use anyhow::{Context, Result};
use cargo_metadata::MetadataCommand;
use git2::{Commit, Deltas, DiffDelta, DiffOptions, Oid, Repository, Sort};
use std::collections::HashMap;
use tracing::{error, info};

fn main() {
    tracing_subscriber::fmt::init();
    match run() {
        Ok(_) => {}
        Err(e) => error!("Error running the build tool: {e:?}"),
    };
}

fn run() -> Result<()> {
    let repo: Repository = Repository::init(".").context("open repo")?;

    // get the head of the current branch
    let head = repo.head().context("get head of repo")?;

    // find the list of commits present locally but not origin/master
    let mut revwalk = repo.revwalk().context("create revwalk")?;

    // we want the last commit present on origin master so we can find what changed in our first commit
    let commit_range = format!("origin/master..HEAD");
    revwalk
        .push_range(&commit_range)
        .context("revwalk commit range")?;
    revwalk.set_sorting(Sort::TOPOLOGICAL | Sort::REVERSE)?;
    let x: Result<Vec<Oid>, git2::Error> = revwalk.into_iter().collect();
    let x = x.context("get oids")?;
    let commits: Result<Vec<Commit>, git2::Error> =
        x.iter().map(|c| repo.find_commit(*c)).collect();
    let commits = commits.context("get commits from oids")?;

    info!("Commits: {commits:?}");

    // get the list of crates in this worktree
    let metadata = MetadataCommand::new().exec().unwrap();
    let members = metadata.workspace_members;
    info!("Members: {members:?}");

    // get the list of updated services for each commit
    // to do this, we'll walk each commit, and check which services changed
    let mut changed_services: HashMap<Commit, Vec<String>> = HashMap::new();
    for commit in &commits {
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
        let deltas: Vec<DiffDelta> = diff.deltas().collect();
        info!("Diff for {commit:?}..{parent_commit:?} is {deltas:?}");
    }

    // suggest a list of version bumps for each service changed

    // allow the user to override the version bump for each service

    // generate the changelog entry

    // commit the changelog and version bumps

    // push to the remote

    // open PR

    Ok(())
}
