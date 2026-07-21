use crate::config::{BumpsConfig, V0Style};
use crate::package::Package;
use crate::pr::bump::Bump;
use crate::pr::run::UpdatedCrate;
use crate::pr::traits::CommitInfo;
use cargo_metadata::semver::Version;
use std::collections::HashMap;
use tracing::info;

/// Parses the conventional-commit header out of a commit summary
/// (`type(scope)!: description`), returning the type, scope, and whether the
/// `!` breaking marker is present. Returns `None` for non-conventional
/// summaries.
fn parse_conventional(summary: &str) -> Option<(&str, Option<&str>, bool)> {
    let (header, _) = summary.split_once(':')?;
    let header = header.trim();
    let (header, breaking) = header
        .strip_suffix('!')
        .map_or((header, false), |rest| (rest, true));
    let (ty, scope) = match header.split_once('(') {
        Some((ty, rest)) => (ty, Some(rest.strip_suffix(')')?)),
        None => (header, None),
    };
    if ty.is_empty() || !ty.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some((ty, scope, breaking))
}

/// How well a configured pattern matches a commit's type and scope:
/// `Some(2)` for an exact `type(scope)` match, `Some(1)` for a bare `type`
/// match (any scope), `None` for no match.
fn pattern_specificity(pattern: &str, ty: &str, scope: Option<&str>) -> Option<u8> {
    match pattern.split_once('(') {
        Some((pattern_ty, rest)) => {
            let pattern_scope = rest.strip_suffix(')')?;
            (pattern_ty == ty && scope == Some(pattern_scope)).then_some(2)
        }
        None => (pattern == ty).then_some(1),
    }
}

/// Looks the commit's type/scope up across the configured lists. The most
/// specific match wins (`chore(release)` beats `chore`); on equal
/// specificity the bigger bump wins, with `skip` losing to everything.
/// `Some(None)` means matched-as-skip, outer `None` means no match at all.
#[allow(clippy::option_option)]
fn best_match(ty: &str, scope: Option<&str>, config: &BumpsConfig) -> Option<Option<Bump>> {
    let lists = [
        (Some(Bump::Major), &config.major),
        (Some(Bump::Minor), &config.minor),
        (Some(Bump::Patch), &config.patch),
        (None, &config.skip),
    ];

    let mut best: Option<(u8, u8, Option<Bump>)> = None;
    for (bump, patterns) in lists {
        // rank orders equal-specificity conflicts: Major > Minor > Patch > skip
        let rank = bump.map_or(0, |b| b as u8 + 1);
        for pattern in patterns {
            let Some(specificity) = pattern_specificity(pattern, ty, scope) else {
                continue;
            };
            if best.is_none_or(|(s, r, _)| (specificity, rank) > (s, r)) {
                best = Some((specificity, rank, bump));
            }
        }
    }
    best.map(|(_, _, bump)| bump)
}

/// Maps one commit to a bump level, or `None` if it matched the `skip` list.
/// A breaking change (`!` header marker or `BREAKING CHANGE:` footer) always
/// means major — even for a type/scope listed under `skip`. Commits matching
/// nothing (including non-conventional summaries) fall back to patch — the
/// crate changed, so it needs at least that.
fn bump_for_commit(commit: &CommitInfo, config: &BumpsConfig) -> Option<Bump> {
    let parsed = parse_conventional(&commit.summary);
    if commit.breaking || parsed.is_some_and(|(_, _, breaking)| breaking) {
        return Some(Bump::Major);
    }
    let Some((ty, scope, _)) = parsed else {
        return Some(Bump::Patch);
    };
    best_match(ty, scope, config).unwrap_or(Some(Bump::Patch))
}

/// Applies the pre-1.0 policy: under the cargo style a 0.x crate maps
/// breaking changes to a minor bump and everything else to a patch bump,
/// mirroring how cargo treats 0.x versions as `0.MAJOR.MINOR`.
const fn adjust_for_v0(bump: Bump, version_major: u64, style: V0Style) -> Bump {
    if version_major > 0 || matches!(style, V0Style::Semver) {
        return bump;
    }
    match bump {
        Bump::Major => Bump::Minor,
        Bump::Minor | Bump::Patch => Bump::Patch,
    }
}

/// The bump a package's commits require, before any v0 adjustment: `None` if every commit matched
/// the `skip` list (and there were commits to check), `Some(Patch)` if there were no attributed
/// commits at all, since the changed-package detection (not attribution) is what's authoritative
/// about a package having changed.
fn required_bump(commits: &[CommitInfo], config: &BumpsConfig) -> Option<Bump> {
    if commits.is_empty() {
        Some(Bump::Patch)
    } else {
        commits
            .iter()
            .filter_map(|c| bump_for_commit(c, config))
            .max()
    }
}

/// Picks each changed package's version bump from its commits without user
/// interaction: the biggest bump any attributed commit maps to wins. A
/// package whose every commit matched the `skip` list is dropped from the
/// release entirely; a package with no attributed commits at all still gets
/// a patch bump, since the changed-package detection (not attribution) is
/// what's authoritative about it having changed.
///
/// `changed` pairs each package with its baseline — the version before this branch's first
/// still-unmerged bump, or its own current version if it has none — alongside its commits (which,
/// for a package with prior sections, are poached: the whole not-yet-released history, not just
/// this run's delta). The bump is applied to that baseline rather than the package's current
/// version, so a fresh commit no more severe than what's already staged doesn't double-bump it —
/// see `Bump::apply_to`.
pub fn select(
    changed: HashMap<Package, (Version, Vec<CommitInfo>)>,
    config: &BumpsConfig,
) -> Vec<UpdatedCrate> {
    let mut updated: Vec<UpdatedCrate> = changed
        .into_iter()
        .filter_map(|(package, (baseline, commits))| {
            let Some(raw) = required_bump(&commits, config) else {
                info!(
                    "{}: all commits matched the skip list, not bumping",
                    package.name
                );
                return None;
            };
            let bump = adjust_for_v0(raw, baseline.major, config.v0);
            let new_version = bump.apply_to(&baseline);
            info!(
                "{}: {} -> {} ({} bump)",
                package.name,
                package.version,
                new_version,
                bump.label()
            );
            Some(UpdatedCrate {
                package,
                new_version,
                commits,
            })
        })
        .collect();
    updated.sort_by(|a, b| a.package.name.cmp(&b.package.name));
    updated
}

#[cfg(test)]
mod tests {
    use super::*;
    use cargo_metadata::semver::Version;

    fn commit(summary: &str) -> CommitInfo {
        CommitInfo {
            summary: summary.to_string(),
            sha1: "0123456789abcdef".to_string(),
            breaking: false,
        }
    }

    fn package(version: &str) -> Package {
        Package {
            path: "a".to_string(),
            name: "a".to_string(),
            version: Version::parse(version).unwrap(),
        }
    }

    // baseline == the package's own current version, i.e. no prior sections poached in — what
    // every test below wants except the ones specifically about poaching.
    fn no_poaching(package: &Package, commits: Vec<CommitInfo>) -> (Version, Vec<CommitInfo>) {
        (package.version.clone(), commits)
    }

    #[test]
    fn parses_plain_scoped_and_breaking_headers() {
        assert_eq!(
            parse_conventional("feat: add x"),
            Some(("feat", None, false))
        );
        assert_eq!(
            parse_conventional("fix(core): y"),
            Some(("fix", Some("core"), false))
        );
        assert_eq!(
            parse_conventional("feat!: drop z"),
            Some(("feat", None, true))
        );
        assert_eq!(
            parse_conventional("feat(api)!: drop z"),
            Some(("feat", Some("api"), true))
        );
    }

    #[test]
    fn non_conventional_summaries_do_not_parse() {
        assert_eq!(parse_conventional("update readme"), None);
        assert_eq!(parse_conventional("!: broken"), None);
        assert_eq!(parse_conventional("a b: spaced type"), None);
        assert_eq!(parse_conventional("feat(unclosed: x"), None);
    }

    #[test]
    fn default_config_maps_types_to_expected_bumps() {
        let config = BumpsConfig::default();
        assert_eq!(
            bump_for_commit(&commit("feat: x"), &config),
            Some(Bump::Minor)
        );
        assert_eq!(
            bump_for_commit(&commit("fix: x"), &config),
            Some(Bump::Patch)
        );
        assert_eq!(
            bump_for_commit(&commit("chore: x"), &config),
            Some(Bump::Patch)
        );
        assert_eq!(
            bump_for_commit(&commit("feat!: x"), &config),
            Some(Bump::Major)
        );
        assert_eq!(
            bump_for_commit(&commit("not conventional"), &config),
            Some(Bump::Patch)
        );
    }

    #[test]
    fn breaking_change_footer_maps_to_major() {
        let config = BumpsConfig::default();
        let mut c = commit("fix: subtle behavior change");
        c.breaking = true;
        assert_eq!(bump_for_commit(&c, &config), Some(Bump::Major));
    }

    #[test]
    fn configured_lists_override_the_defaults() {
        let config = BumpsConfig {
            major: vec!["feat".to_string()],
            minor: vec!["fix".to_string()],
            ..BumpsConfig::default()
        };
        assert_eq!(
            bump_for_commit(&commit("feat: x"), &config),
            Some(Bump::Major)
        );
        assert_eq!(
            bump_for_commit(&commit("fix: x"), &config),
            Some(Bump::Minor)
        );
    }

    #[test]
    fn scoped_pattern_beats_bare_type() {
        // chore -> patch, but chore(release) -> skipped
        let config = BumpsConfig {
            patch: vec!["chore".to_string()],
            skip: vec!["chore(release)".to_string()],
            ..BumpsConfig::default()
        };
        assert_eq!(
            bump_for_commit(&commit("chore: tidy"), &config),
            Some(Bump::Patch)
        );
        assert_eq!(
            bump_for_commit(&commit("chore(release): 1.2.3"), &config),
            None
        );
        // other scopes still fall through to the bare-type match
        assert_eq!(
            bump_for_commit(&commit("chore(deps): bump serde"), &config),
            Some(Bump::Patch)
        );
    }

    #[test]
    fn scoped_pattern_can_also_raise_the_bump() {
        let config = BumpsConfig {
            minor: vec!["chore(api)".to_string()],
            skip: vec!["chore".to_string()],
            patch: vec![],
            ..BumpsConfig::default()
        };
        assert_eq!(bump_for_commit(&commit("chore: tidy"), &config), None);
        assert_eq!(
            bump_for_commit(&commit("chore(api): expose thing"), &config),
            Some(Bump::Minor)
        );
    }

    #[test]
    fn breaking_marker_wins_over_skip() {
        let config = BumpsConfig {
            skip: vec!["chore(release)".to_string()],
            ..BumpsConfig::default()
        };
        assert_eq!(
            bump_for_commit(&commit("chore(release)!: drop old artifacts"), &config),
            Some(Bump::Major)
        );
    }

    #[test]
    fn cargo_style_caps_bumps_below_one_zero() {
        assert_eq!(adjust_for_v0(Bump::Major, 0, V0Style::Cargo), Bump::Minor);
        assert_eq!(adjust_for_v0(Bump::Minor, 0, V0Style::Cargo), Bump::Patch);
        assert_eq!(adjust_for_v0(Bump::Patch, 0, V0Style::Cargo), Bump::Patch);
    }

    #[test]
    fn semver_style_and_post_one_zero_apply_bumps_as_is() {
        assert_eq!(adjust_for_v0(Bump::Major, 0, V0Style::Semver), Bump::Major);
        assert_eq!(adjust_for_v0(Bump::Minor, 0, V0Style::Semver), Bump::Minor);
        assert_eq!(adjust_for_v0(Bump::Major, 1, V0Style::Cargo), Bump::Major);
    }

    #[test]
    fn select_takes_the_max_bump_across_commits() {
        let pkg = package("1.2.3");
        let commits = vec![commit("fix: a"), commit("feat: b"), commit("chore: c")];
        let changed = HashMap::from([(pkg.clone(), no_poaching(&pkg, commits))]);
        let updated = select(changed, &BumpsConfig::default());
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].new_version, Version::parse("1.3.0").unwrap());
    }

    #[test]
    fn select_applies_the_v0_policy() {
        let pkg = package("0.1.0");
        let changed = HashMap::from([(
            pkg.clone(),
            no_poaching(&pkg, vec![commit("feat!: breaking")]),
        )]);
        let updated = select(changed, &BumpsConfig::default());
        assert_eq!(updated[0].new_version, Version::parse("0.2.0").unwrap());

        let changed =
            HashMap::from([(pkg.clone(), no_poaching(&pkg, vec![commit("feat: minor")]))]);
        let updated = select(changed, &BumpsConfig::default());
        assert_eq!(updated[0].new_version, Version::parse("0.1.1").unwrap());
    }

    #[test]
    fn select_drops_packages_whose_every_commit_is_skipped() {
        let config = BumpsConfig {
            skip: vec!["chore(release)".to_string()],
            ..BumpsConfig::default()
        };
        let pkg = package("0.1.0");
        let changed = HashMap::from([(
            pkg.clone(),
            no_poaching(&pkg, vec![commit("chore(release): 0.1.0")]),
        )]);
        assert!(select(changed, &config).is_empty());

        // a non-skipped commit alongside the skipped one keeps the package in
        let changed = HashMap::from([(
            pkg.clone(),
            no_poaching(
                &pkg,
                vec![commit("chore(release): 0.1.0"), commit("fix: a")],
            ),
        )]);
        let updated = select(changed, &config);
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].new_version, Version::parse("0.1.1").unwrap());
    }

    #[test]
    fn packages_without_commits_get_a_patch_bump() {
        let pkg = package("0.1.0");
        let changed = HashMap::from([(pkg.clone(), no_poaching(&pkg, Vec::new()))]);
        let updated = select(changed, &BumpsConfig::default());
        assert_eq!(updated[0].new_version, Version::parse("0.1.1").unwrap());
    }

    #[test]
    fn select_applies_the_bump_to_the_baseline_not_the_current_version() {
        // package currently sits at 1.3.0 on disk (already staged by a prior, still-unmerged
        // bump), but its true baseline — before that bump — was 1.2.5. A fresh patch-level commit
        // that doesn't exceed the already-staged minor bump must not stack another bump on top of
        // the current 1.3.0.
        let pkg = package("1.3.0");
        let baseline = Version::parse("1.2.5").unwrap();
        let poached_plus_fresh = vec![commit("feat: b"), commit("fix: a")];
        let changed = HashMap::from([(pkg.clone(), (baseline, poached_plus_fresh))]);

        let updated = select(changed, &BumpsConfig::default());
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].new_version, Version::parse("1.3.0").unwrap());
        assert_eq!(updated[0].new_version, pkg.version, "should stay put");
    }

    #[test]
    fn select_escalates_past_the_baseline_when_a_fresh_commit_is_more_severe() {
        // same staged state as above, but this time the fresh commit is breaking — the max
        // severity across the whole (poached) history now exceeds the already-staged minor bump.
        let pkg = package("1.3.0");
        let baseline = Version::parse("1.2.5").unwrap();
        let poached_plus_fresh = vec![commit("feat: b"), commit("feat!: breaking")];
        let changed = HashMap::from([(pkg, (baseline, poached_plus_fresh))]);

        let updated = select(changed, &BumpsConfig::default());
        assert_eq!(updated[0].new_version, Version::parse("2.0.0").unwrap());
    }
}
