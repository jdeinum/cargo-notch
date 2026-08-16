use crate::{
    commit::{auto::bump_for_commit, bump::Bump},
    config::BumpsConfig,
    utils::commits::CommitInfo,
};

/// The FSM here is responsible for the following:
///
/// 1. Track the commits for a SINGLE package
/// 2. Track all version bumps for a SINGLE package
pub struct NotchFsm {
    bump_config: BumpsConfig,
    current_bump: Option<Bump>,
    commits: Vec<CommitInfo>,
}

impl NotchFsm {
    pub const fn new(bump_config: BumpsConfig) -> Self {
        Self {
            bump_config,
            current_bump: None,
            commits: Vec::new(),
        }
    }

    pub fn handle_commits(&mut self, commits: &[CommitInfo]) {
        // first, we can just append these commits to our commit list because we already know
        // they're part of this package
        self.commits.extend(commits.iter().cloned());

        // get the biggest bump type across these commits, if None, we can safely return since
        // none of them bump the version
        let bump_type = commits
            .iter()
            .filter_map(|x| bump_for_commit(x, &self.bump_config))
            .max();
        let Some(bump_type) = bump_type else { return };

        // now we check if our current version bump is sufficent
        match self.current_bump {
            // The commits require a greater bump change than we currently have
            Some(x) if bump_type > x => self.current_bump = Some(bump_type),
            // The commits can safely be folded under the existing bump
            Some(_) => {}
            // No previous bump, just set it to the commits' bump type
            None => self.current_bump = Some(bump_type),
        }
    }

    /// This FSM's package's commits, regardless of whether any of them contribute a bump — unlike
    /// `dump`, which drops them alongside a `None` bump, this is what a UI showing "here's
    /// everything attributed to this package" (even with nothing suggested yet) should read from.
    pub fn commits(&self) -> &[CommitInfo] {
        &self.commits
    }

    /// The bump this FSM's commits currently suggest, if any.
    pub const fn suggested_bump(&self) -> Option<Bump> {
        self.current_bump
    }

    /// Dump our current bump and commit list
    ///
    /// NOTE: If we have a commit that doesn't affect the package version, that commit will not show
    /// up ANYWHERE, thus is kind of just a ghost commit.
    pub fn dump(&self) -> Option<(Bump, Vec<CommitInfo>)> {
        self.current_bump.map(|bump| (bump, self.commits.clone()))
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use quickcheck::Arbitrary;
    use quickcheck_macros::quickcheck;

    #[test]
    fn no_commits_returns_none() {
        let fsm = NotchFsm::new(BumpsConfig::default());
        assert_eq!(fsm.dump(), None);
    }

    #[test]
    fn one_commit_returns_that_one() {
        let mut fsm = NotchFsm::new(BumpsConfig::default());
        let commits = vec![CommitInfo::new(
            "fix: fixing something".into(),
            "abcdef123456789".into(),
            false,
        )];
        fsm.handle_commits(&commits);
        assert_eq!(fsm.dump(), Some((Bump::Patch, commits)));
    }

    #[test]
    fn second_commit_no_bump_required() {
        let mut fsm = NotchFsm::new(BumpsConfig::default());
        let commits = vec![
            CommitInfo::new(
                "fix: fixing something".into(),
                "abcdef123456789".into(),
                false,
            ),
            CommitInfo::new(
                "fix: fixing something new".into(),
                "abcdef123456788".into(),
                false,
            ),
        ];
        fsm.handle_commits(&commits);
        assert_eq!(fsm.dump(), Some((Bump::Patch, commits)));
    }

    #[test]
    fn second_commit_bump_required() {
        let mut fsm = NotchFsm::new(BumpsConfig::default());
        let commits = vec![
            CommitInfo::new(
                "fix: fixing something".into(),
                "abcdef123456789".into(),
                false,
            ),
            CommitInfo::new(
                "feat: adding something new".into(),
                "abcdef123456788".into(),
                false,
            ),
        ];
        fsm.handle_commits(&commits);
        assert_eq!(fsm.dump(), Some((Bump::Minor, commits)));
    }

    // with our base cases out of the way, we can now look to add some property based tests. We'll
    // gate them behind an env var, see https://matklad.github.io/2021/05/31/how-to-test.html for why.
    //
    // Some properties that should be upheld:
    // 1. Given a set of commits, the bump we have cannot exceed the max bump from the contained commits
    // 2. We can never somehow have more commits than we put in. We can however, get out less than
    //    we put in though, because if there is no bump, then no commits are returned, and they are
    //    lost to the void

    // We will use a single property based test that checks both of these invarients, but we will
    // need to generate a generator for commit summaries, because they require some structure to them.
    impl Arbitrary for CommitInfo {
        fn arbitrary(g: &mut quickcheck::Gen) -> Self {
            // first we select from our default types
            let types = ["fix", "feat", "refactor", "chore", "docs"];
            let ty = g.choose(&types).unwrap();

            // pick the scope
            // in this case theres a 50% chance of including a scope
            let scopes = ["core", "cli", "parser"];
            let scope = bool::arbitrary(g)
                .then(|| *g.choose(&scopes).unwrap())
                .map_or(String::new(), |x| format!("({x})"));

            // around 33% chance of breaking change
            let breaking = u16::arbitrary(g) % 10 > 3;
            let breaking_msg = if breaking { "!" } else { "" };

            // now the message is just a random string
            let msg = String::arbitrary(g);

            Self {
                summary: format!("{ty}{scope}{breaking_msg}: {msg}"),
                breaking,
                sha1: "abcdefg123456".to_string(),
            }
        }
    }

    #[quickcheck]
    #[allow(clippy::needless_pass_by_value)]
    fn check(v: Vec<CommitInfo>) {
        if std::env::var("PBT_ENABLED").is_ok() {
            let config = BumpsConfig::default();
            let max = v
                .iter()
                .map(|c| bump_for_commit(c, &config))
                .max()
                .flatten();

            let mut package_fsm = NotchFsm::new(BumpsConfig::default());
            package_fsm.handle_commits(&v);

            assert_eq!(package_fsm.dump().map(|(bump, _)| bump), max);
        }
    }
}
