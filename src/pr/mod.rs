mod tui;

use crate::config::{self, ReleaseConfig};
use crate::error::{Error, Result};
use crate::workspace::{Crate, get_cleaned_members};
use anyhow::Context;
use cargo_metadata::semver::Version;
use git2::{
    Commit, Cred, DiffOptions, IndexAddOption, Oid, PushOptions, RemoteCallbacks, Repository,
    Signature, Sort,
};
use octocrab::Octocrab;
use secrecy::ExposeSecret;
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    process::Command,
};
use tracing::debug;

// Notch identity
const NOTCH_COMMIT_MESSAGE: &str = "chore(notch): changelog + version bump";
const NOTCH_COMMITTER_NAME: &str = "notch";
const NOTCH_COMMITTER_EMAIL: &str = "notch@noreply.notch-release";
const NOTCH_TRAILER_KEY: &str = "Notch-Bump";

pub fn run() -> Result<()> {
    let pwd = std::env::current_dir().context("get current dir")?;
    let cleaned_members = get_cleaned_members(&pwd).context("get cleaned members")?;
    let config = config::load(&pwd).context("load notch.toml")?;

    let Some(token) = config.repo.token.clone() else {
        return Err(Error::msg("No token provided"));
    };

    let repo: Repository = Repository::init(".").context("open repo")?;
    let (owner, repo_name) =
        config::resolve_owner_repo(&repo, &config.repo).context("resolve owner/repo")?;

    fetch_remote(&repo, &config.release).context("fetch remote")?;

    let all_commits = get_commits(&repo, &config.release).context("get commits")?;

    // If a previous run already left a bump commit somewhere in this range (whether or not new
    // commits have since landed on top of it), diff and generate the changelog against it instead
    // the production branch. This prevents
    let last_notch_commit = find_last_notch_commit(&all_commits);

    // Commits attributed to crates (for the TUI and PR body) should only be the ones new since that
    // prior bump, not the full history back to upstream.
    let commits: Vec<Commit> = match &last_notch_commit {
        Some(marker) => all_commits
            .into_iter()
            .skip_while(|c| c.id() != marker.id())
            .skip(1)
            .collect(),
        None => all_commits,
    };

    let changed_crates = get_changed_crates(
        &repo,
        &config.release,
        &cleaned_members,
        last_notch_commit.as_ref(),
    )
    .context("get changed crates")?;

    if changed_crates.is_empty() {
        println!("No crates to update, not creating commits or a release pr");
        return Ok(());
    }

    let changed_crates_with_commits = attribute_commits_to_crates(&repo, &commits, &changed_crates)
        .context("attribute commits to changed crates")?;

    let mut packages: Vec<tui::PackageItem> = changed_crates_with_commits
        .into_iter()
        .map(|(ccrate, commits)| tui::PackageItem::new(ccrate, commits))
        .collect();
    packages.sort_by(|a, b| a.ccrate().name.cmp(&b.ccrate().name));

    let Some(res) = tui::run(packages).context("select version bumps")? else {
        println!("Cancelled, no changes made");
        return Ok(());
    };

    let changelog_range = changelog_range(&config.release, last_notch_commit.as_ref());
    for updated_crate in &res {
        update_package(updated_crate, &changelog_range).context("update the package")?;
    }

    // cargo generate-lockfile so we update everything we need
    generate_lockfile().context("generate new lockfile")?;

    // commit changes
    commit_changes(&repo, &res).context("commit changes to the repo")?;

    // push to the remote
    // requires we have access to the SSH agent on our system, not quite sure how to do that yet
    push_current_branch(&repo, &config.release).context("push current branch")?;

    // open the PR
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("spawn runtime")?;

    rt.block_on(open_pr(
        &owner,
        &repo_name,
        &repo,
        &config.release,
        token.expose_secret(),
        &res,
    ))
    .context("open PR on runtime")?;

    Ok(())
}

// Updates the local `<remote>/<default_branch>` tracking ref before we diff against it. Without
// this, a stale local ref makes `commit_range()` include far more history than is actually
// unmerged, and every crate ever touched in that stale range gets (incorrectly) flagged as changed.
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

    debug!("Commits: {commits:?}");
    Ok(commits)
}

// Determines which crates actually differ between HEAD and `base_override` (if given — see
// `find_last_notch_commit`) or otherwise the merge-base with `<remote>/<default_branch>` — i.e.
// what's really different from the current upstream state, regardless of how the branch's history
// got there (merges, reverts, rebases, ...). This is the authoritative source for which crates need
// a version bump.
//
// Walking each commit's diff against its immediate parent and unioning the touched paths doesn't
// work: a merge commit's diff against its first parent pulls in everything the *other* parent
// brought in (e.g. an entire release worth of changes from `master`), and reverted changes still
// get counted even though the net diff against upstream is zero.
fn get_changed_crates(
    repo: &Repository,
    release: &ReleaseConfig,
    crates: &[Crate],
    base_override: Option<&Commit>,
) -> Result<HashSet<Crate>> {
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

    let changed_crates: HashSet<Crate> = crates
        .iter()
        .filter(|c| c.path == "." || files.iter().any(|f| f.starts_with(&c.path)))
        .cloned()
        .collect();

    for c_crate in &changed_crates {
        debug!("Crate {} changed", c_crate.path);
    }

    Ok(changed_crates)
}

/// A commit's display-relevant data, read out of `git2::Commit` up front so
/// downstream code (the TUI, the PR body) doesn't need to hold a borrow on
/// the `Repository` or the original `Commit`.
#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub short_id: String,
    pub summary: String,
}

impl CommitInfo {
    fn from_commit(commit: &Commit) -> Self {
        let summary = commit.summary().ok().flatten().unwrap_or("<no summary>");
        Self {
            short_id: commit.id().to_string()[..7].to_string(),
            summary: summary.to_string(),
        }
    }
}

// Attributes each already-confirmed changed crate to the non-merge commits whose own diff touched
// its path, purely for displaying to the user which commits are likely responsible — this is
// informational, not authoritative (see `get_changed_crates`).
fn attribute_commits_to_crates(
    repo: &Repository,
    commits: &[Commit],
    changed_crates: &HashSet<Crate>,
) -> Result<HashMap<Crate, Vec<CommitInfo>>> {
    let mut attributed: HashMap<Crate, Vec<CommitInfo>> = changed_crates
        .iter()
        .map(|c| (c.clone(), Vec::new()))
        .collect();

    for commit in commits {
        // merge commits are excluded here for the same reason as in
        // `get_changed_crates`: their diff against a single parent isn't a
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

        for cratem in changed_crates {
            if cratem.path == "." || files.iter().any(|f| f.starts_with(&cratem.path)) {
                attributed
                    .get_mut(cratem)
                    .expect("populated from changed_crates above")
                    .push(CommitInfo::from_commit(commit));
            }
        }
    }

    for (c_crate, commits) in &attributed {
        debug!("Crate {} changed from the following commits:", c_crate.path);
        for c in commits {
            debug!("{}", c.summary);
        }
    }

    Ok(attributed)
}

// Fixed identity for notch's own commits, so they're recognizable in `git log`/`git blame` and,
// combined with the `Notch-Bump` trailer, can't be mistaken for a human commit that happens to
// start the same way.
fn notch_signature<'a>() -> Result<Signature<'a>> {
    Signature::now(NOTCH_COMMITTER_NAME, NOTCH_COMMITTER_EMAIL).context("build notch signature")
}

// `crate@version` pairs for every crate this run bumped, recorded as a trailer so a later run can
// recover exactly what a prior bump commit did without needing a second source of truth.
fn build_bump_trailer(updated: &[UpdatedCrate]) -> String {
    let pairs = updated
        .iter()
        .map(|c| format!("{}@{}", c.ccrate.name, c.new_version))
        .collect::<Vec<_>>()
        .join(",");
    format!("{NOTCH_TRAILER_KEY}: {pairs}")
}

// A commit is notch's own release commit only if both the committer identity and the trailer match
// — either alone could plausibly be forged or coincidental, but not both together.
fn is_notch_commit(commit: &Commit) -> bool {
    let committer_matches = commit
        .committer()
        .email()
        .is_ok_and(|e| e == NOTCH_COMMITTER_EMAIL);
    let has_trailer = commit.message().is_ok_and(|m| {
        m.lines()
            .any(|l| l.starts_with(&format!("{NOTCH_TRAILER_KEY}:")))
    });
    committer_matches && has_trailer
}

// `commits` is oldest-first, so the last match is the most recent bump — the one a rerun should
// diff against.
fn find_last_notch_commit<'a>(commits: &[Commit<'a>]) -> Option<Commit<'a>> {
    commits.iter().rev().find(|c| is_notch_commit(c)).cloned()
}

// The range git-cliff should scan: since the last notch bump if one exists in this branch's
// history, otherwise the same upstream-relative range used everywhere else.
fn changelog_range(release: &ReleaseConfig, last_notch_commit: Option<&Commit>) -> String {
    last_notch_commit.map_or_else(|| release.commit_range(), |c| format!("{}..HEAD", c.id()))
}

// Commits current changes for this repo
// TODO: We should only commit the files notch actually touches, and maybe check if they are already
// dirty and prevent usage if dirty?
fn commit_changes(repo: &Repository, updated: &[UpdatedCrate]) -> Result<()> {
    // commit the changelog and version bumps
    let mut index = repo.index().context("get index for repo")?;
    index
        .add_all(std::iter::once(&"."), IndexAddOption::DEFAULT, None)
        .context("add all files to the index")?;
    index.write().context("write index to disk")?;
    let sig = notch_signature()?;
    let tree = repo
        .find_tree(index.write_tree().context("write tree for index")?)
        .context("find tree")?;
    let parent = repo
        .head()
        .context("get head of branch")?
        .peel_to_commit()
        .context("convert ref commit")?;

    let message = format!("{NOTCH_COMMIT_MESSAGE}\n\n{}", build_bump_trailer(updated));

    repo.commit(Some("HEAD"), &sig, &sig, &message, &tree, &[&parent])
        .context("create the commit")?;

    Ok(())
}

// update the lockfile for the current project
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

// Generate the changelog for the provided commit range
fn generate_changelog(tag: &str, crate_path: &str, commit_range: &str) -> Result<()> {
    let res = Command::new("git")
        .args([
            "cliff",
            "--tag",
            tag,
            "--prepend",
            &format!("{crate_path}/CHANGELOG.md"),
            commit_range,
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

pub struct UpdatedCrate {
    pub ccrate: Crate,
    pub new_version: Version,
    pub commits: Vec<CommitInfo>,
}

fn update_package(updated_crate: &UpdatedCrate, commit_range: &str) -> Result<()> {
    // create a backup of the current Cargo.toml
    let real_file = format!("{}/Cargo.toml", updated_crate.ccrate.path);
    let tmp_file = format!("{}/Cargo.toml.bak", updated_crate.ccrate.path);

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
        &format!("version = \"{}\"", updated_crate.ccrate.version.version),
        &format!("version = \"{}\"", updated_crate.new_version),
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
    generate_changelog(
        &updated_crate.new_version.to_string(),
        &updated_crate.ccrate.path,
        commit_range,
    )
    .context("generate changelog")?;
    Ok(())
}

fn push_current_branch(repo: &Repository, release: &ReleaseConfig) -> Result<()> {
    let head = repo.head().context("get head")?;
    let branch = head.name().context("get head name")?;

    let mut remote = repo.find_remote(&release.remote).context("get remote")?;

    debug!("Found remote {}", release.remote);

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
    updated_crates: &[UpdatedCrate],
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
    debug!(
        "Creating PR from {upstream_branch_name} into {}",
        release.default_branch
    );

    let octocrab = Octocrab::builder()
        .personal_token(token)
        .build()
        .context("build octocrab")?;

    let (title, body) =
        get_pr_title_and_description(updated_crates).context("get title pr and description")?;

    let pr = octocrab
        .pulls(owner, repo_name)
        .create(title, name, &release.default_branch)
        .body(body)
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

fn get_pr_title_and_description(updated_crates: &[UpdatedCrate]) -> Result<(String, String)> {
    fn bump_line(c: &UpdatedCrate) -> String {
        format!(
            "chore: bumping {} from {} to {}",
            c.ccrate.name, c.ccrate.version.version, c.new_version
        )
    }

    fn commit_list(c: &UpdatedCrate) -> String {
        c.commits
            .iter()
            .map(|commit| format!("- {} {}", commit.short_id, commit.summary))
            .collect::<Vec<_>>()
            .join("\n")
    }

    let title = match updated_crates {
        [] => return Err(Error::msg("No updated crates, shouldn't be creating a PR!")),
        [c] => bump_line(c),
        _ => "chore: bumping package versions".to_string(),
    };

    let body = updated_crates
        .iter()
        .map(|c| format!("{}\n{}\n", bump_line(c), commit_list(c)))
        .collect::<Vec<_>>()
        .join("\n");

    Ok((title, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::MyVersion;
    use std::fs;

    // Builds a throwaway repo with a single commit authored by `sig`, so
    // `is_notch_commit`/`find_last_notch_commit` can be exercised against a
    // real `git2::Commit` rather than a hand-built struct.
    fn repo_with_commit(dir_suffix: &str, message: &str, sig: &Signature) -> (Repository, Oid) {
        let dir =
            std::env::temp_dir().join(format!("notch-pr-test-{dir_suffix}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let repo = Repository::init(&dir).unwrap();
        fs::write(dir.join("file.txt"), "hello").unwrap();

        let mut index = repo.index().unwrap();
        index.add_path(Path::new("file.txt")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();

        let oid = repo
            .commit(Some("HEAD"), sig, sig, message, &tree, &[])
            .unwrap();
        drop(tree);
        (repo, oid)
    }

    fn updated_crate(name: &str, from: (u64, u64, u64), to: Version) -> UpdatedCrate {
        UpdatedCrate {
            ccrate: Crate {
                path: name.to_string(),
                name: name.to_string(),
                version: MyVersion {
                    version: Version::new(from.0, from.1, from.2),
                },
            },
            new_version: to,
            commits: vec![],
        }
    }

    #[test]
    fn build_bump_trailer_lists_every_crate_updated() {
        let updated = vec![
            updated_crate("foo", (1, 0, 0), Version::new(1, 1, 0)),
            updated_crate("bar", (0, 4, 0), Version::new(0, 5, 0)),
        ];

        assert_eq!(
            build_bump_trailer(&updated),
            "Notch-Bump: foo@1.1.0,bar@0.5.0"
        );
    }

    #[test]
    fn notch_commit_is_identified_by_identity_and_trailer_together() {
        let updated = vec![updated_crate("foo", (1, 0, 0), Version::new(1, 1, 0))];
        let message = format!("{NOTCH_COMMIT_MESSAGE}\n\n{}", build_bump_trailer(&updated));

        let (repo, oid) = repo_with_commit("notch", &message, &notch_signature().unwrap());
        let commit = repo.find_commit(oid).unwrap();
        assert!(is_notch_commit(&commit));

        let _ = fs::remove_dir_all(repo.workdir().unwrap());
    }

    #[test]
    fn human_commit_with_the_same_wording_is_not_mistaken_for_notchs() {
        let updated = vec![updated_crate("foo", (1, 0, 0), Version::new(1, 1, 0))];
        let message = format!("{NOTCH_COMMIT_MESSAGE}\n\n{}", build_bump_trailer(&updated));
        let human_sig = Signature::now("a human", "human@example.com").unwrap();

        let (repo, oid) = repo_with_commit("human", &message, &human_sig);
        let commit = repo.find_commit(oid).unwrap();
        assert!(!is_notch_commit(&commit));

        let _ = fs::remove_dir_all(repo.workdir().unwrap());
    }

    #[test]
    fn notch_identity_without_the_trailer_is_not_mistaken_for_a_bump_commit() {
        let (repo, oid) = repo_with_commit(
            "no-trailer",
            "chore(notch): unrelated",
            &notch_signature().unwrap(),
        );
        let commit = repo.find_commit(oid).unwrap();
        assert!(!is_notch_commit(&commit));

        let _ = fs::remove_dir_all(repo.workdir().unwrap());
    }

    #[test]
    fn find_last_notch_commit_picks_the_one_closest_to_head() {
        let updated = vec![updated_crate("foo", (1, 0, 0), Version::new(1, 1, 0))];
        let message = format!("{NOTCH_COMMIT_MESSAGE}\n\n{}", build_bump_trailer(&updated));
        let sig = notch_signature().unwrap();

        // Two notch commits with a human commit in between — oldest-first,
        // as `get_commits` returns them.
        let (repo, first_oid) = repo_with_commit("multi", &message, &sig);
        let human_sig = Signature::now("a human", "human@example.com").unwrap();
        let human_commit = repo.find_commit(first_oid).unwrap();
        let mut index = repo.index().unwrap();
        fs::write(repo.workdir().unwrap().join("file2.txt"), "again").unwrap();
        index.add_path(Path::new("file2.txt")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let second_oid = repo
            .commit(
                Some("HEAD"),
                &human_sig,
                &human_sig,
                "feat: add a thing",
                &tree,
                &[&human_commit],
            )
            .unwrap();

        let second_notch_commit = repo.find_commit(second_oid).unwrap();
        fs::write(repo.workdir().unwrap().join("file.txt"), "changed").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("file.txt")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let third_oid = repo
            .commit(
                Some("HEAD"),
                &sig,
                &sig,
                &message,
                &tree,
                &[&second_notch_commit],
            )
            .unwrap();

        let commits = vec![
            repo.find_commit(first_oid).unwrap(),
            repo.find_commit(second_oid).unwrap(),
            repo.find_commit(third_oid).unwrap(),
        ];

        let found = find_last_notch_commit(&commits).unwrap();
        assert_eq!(found.id(), third_oid);

        let _ = fs::remove_dir_all(repo.workdir().unwrap());
    }

    #[test]
    fn changelog_range_falls_back_to_upstream_range_without_a_marker() {
        let release = ReleaseConfig::default();
        assert_eq!(changelog_range(&release, None), "origin/master..HEAD");
    }

    #[test]
    fn changelog_range_uses_the_marker_commit_when_present() {
        let (repo, oid) =
            repo_with_commit("range", NOTCH_COMMIT_MESSAGE, &notch_signature().unwrap());
        let commit = repo.find_commit(oid).unwrap();
        let release = ReleaseConfig::default();

        assert_eq!(
            changelog_range(&release, Some(&commit)),
            format!("{oid}..HEAD")
        );

        let _ = fs::remove_dir_all(repo.workdir().unwrap());
    }
}
