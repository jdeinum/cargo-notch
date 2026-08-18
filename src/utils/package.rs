use cargo_metadata::semver::Version;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// The repo-relative paths a package considers its own: everything under `tracked`, minus anything
/// under `excluded`.
///
/// Owned by the package rather than reconstructed at each call site, because "which files belong
/// to this package" is ecosystem knowledge — a cargo member owns its directory, but a workspace
/// root that is *also* a package owns the whole repo minus its members, and neither shape is
/// derivable from a bare directory string. Commit attribution varies by VCS, not by ecosystem, so
/// it has no business knowing either; it just asks.
///
/// Both lists are prefix-matched with [`Path::starts_with`], which compares whole path components.
/// That's load-bearing: a plain string prefix would let `crates/ab` match the package at
/// `crates/a`. Where the two lists overlap, the more specific prefix wins — see
/// [`matches`](PathSpec::matches).
#[derive(Debug, Clone, Default)]
pub struct PathSpec {
    tracked: Vec<PathBuf>,
    excluded: Vec<PathBuf>,
}

impl PathSpec {
    /// A spec owning everything under `dir` and nothing excluded yet.
    ///
    /// The repo root is spelled `"."` by [`Package::path`] but has to become the *empty* path
    /// here: `Path::starts_with(".")` is false for every relative path a git diff produces, while
    /// `starts_with("")` is true for all of them — which is precisely what "this package owns the
    /// whole repo" means. Encoding it that way is what lets [`matches`](PathSpec::matches) stay a
    /// plain prefix test instead of carrying a `== "."` special case, which is cargo-shaped
    /// knowledge that has no place in ecosystem-agnostic code.
    pub fn rooted_at(dir: &str) -> Self {
        let root = if dir == "." {
            PathBuf::new()
        } else {
            PathBuf::from(dir)
        };
        Self {
            tracked: vec![root],
            excluded: Vec::new(),
        }
    }

    /// Carves `path` out of what this package tracks. Nesting is the point: an exclude is
    /// normally *inside* a tracked prefix (a workspace member sitting under the root package, or
    /// a `benches/` directory the user doesn't want to release for), so the two lists overlap by
    /// design and [`matches`](PathSpec::matches) resolves the overlap.
    pub fn exclude(&mut self, path: impl Into<PathBuf>) {
        if let Some(path) = narrower_than_everything(path.into()) {
            self.excluded.push(path);
        }
    }

    /// Carves `path` back *into* what this package tracks, out from under any exclude covering it
    /// — the `except tests/compat/` half of `ignore tests/, except tests/compat/`. Expressible
    /// only because [`matches`](PathSpec::matches) resolves by specificity rather than by which
    /// list a prefix came from.
    pub fn track(&mut self, path: impl Into<PathBuf>) {
        if let Some(path) = narrower_than_everything(path.into()) {
            self.tracked.push(path);
        }
    }

    /// Whether a change to `file` counts as a change to this package.
    ///
    /// The most specific prefix wins, whichever list it came from, so the two lists are the same
    /// kind of statement with opposite polarity rather than one overriding the other. That's what
    /// makes an exclude nested in a tracked path work (`crates/a/benches` inside `crates/a`) *and*
    /// a track nested in an exclude (`tests/compat` inside `tests`), to any depth, with no
    /// dependence on the order the two lists were built in.
    ///
    /// Note the rule is only distinguishable from "an exclude always wins" once something nests a
    /// tracked path inside an excluded one. Nothing in the ecosystem layer does; it exists so
    /// `[tracking] include` can.
    pub fn matches(&self, file: &Path) -> bool {
        // Among prefixes that all match the same file, component count is a total order: any two
        // of them necessarily nest one inside the other, so "longer" and "more specific" are the
        // same thing and there is nothing left to disambiguate.
        let most_specific = |prefixes: &[PathBuf]| {
            prefixes
                .iter()
                .filter(|prefix| file.starts_with(prefix))
                .map(|prefix| prefix.components().count())
                .max()
        };

        match (most_specific(&self.tracked), most_specific(&self.excluded)) {
            // A tie is the same path named in both lists. Excluding is the narrower reading of a
            // contradictory instruction, and the one whose failure mode is a missing release
            // rather than a spurious one.
            (Some(tracked), Some(excluded)) => tracked > excluded,
            (Some(_), None) => true,
            (None, _) => false,
        }
    }
}

// An empty prefix matches every path, so a stray `""` — a config entry of `""` or `"."`, which
// `Package::join` turns into the package's own directory — would silently stop a package ever
// being released, or (as a track) hand it every file in the repo. Refusing it beats debugging it.
fn narrower_than_everything(path: PathBuf) -> Option<PathBuf> {
    (!path.as_os_str().is_empty()).then_some(path)
}

#[derive(Debug, Clone)]
pub struct Package {
    /// Repo-relative directory containing the package's manifest.
    pub path: String,
    /// The package's actual name, as its manifest declares it.
    pub name: String,
    pub version: Version,
    /// Repo-relative path to the manifest file this package's version lives in. Supplied by the
    /// [`Ecosystem`](crate::utils::packages::Ecosystem) that discovered the package, so nothing
    /// outside `utils::packages` has to know whether that file is a `Cargo.toml`, a
    /// `package.json`, or a `pyproject.toml` — reconstructing it at each call site is how that
    /// knowledge leaked into otherwise ecosystem-agnostic code.
    pub manifest: PathBuf,
    /// Which files count as changes to this package. Defaults to "everything under `path`"; the
    /// discovering [`Ecosystem`](crate::utils::packages::Ecosystem) narrows it for packages that
    /// contain other packages, and `[tracking]` config narrows it further.
    pub paths: PathSpec,
}

// A package is identified by its manifest, and nothing else: exactly one package is defined per
// manifest file, so it's a true key, and unlike `version` it doesn't move under us mid-run.
// Deriving these would fold `version` into the identity — and changing that field is the entire
// point of this tool, so a `HashMap<Package, _>` built before a bump could no longer be looked up
// with the same package after one. Nothing needs whole-struct equality (`tag::compute_tags`, the
// one place that compares packages across commits, keys by path and diffs `Version` directly), so
// there's no reason to keep the derive's stricter notion around for it.
impl PartialEq for Package {
    fn eq(&self, other: &Self) -> bool {
        self.manifest == other.manifest
    }
}

impl Eq for Package {}

impl Hash for Package {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.manifest.hash(state);
    }
}

impl Package {
    /// A package tracking everything under `path`. Constructing one this way rather than through a
    /// struct literal is what guarantees `paths` can never disagree with `path`; narrowing it is
    /// then an explicit follow-up call rather than something a caller can forget to fill in.
    pub fn new(path: String, name: String, version: Version, manifest: PathBuf) -> Self {
        let paths = PathSpec::rooted_at(&path);
        Self {
            path,
            name,
            version,
            manifest,
            paths,
        }
    }

    /// Joins this package's path with a filename to get a path relative to
    /// the repo root. A root-level package is normalized to "." (see
    /// `get_cleaned_members`), so naive concatenation would produce a
    /// leading "./" that libgit2 rejects (e.g. from `status_file` or
    /// `Index::add_path`).
    pub fn join(&self, file: &str) -> String {
        if self.path == "." {
            file.to_string()
        } else {
            format!("{}/{file}", self.path)
        }
    }

    /// Whether a change to `file` counts as a change to this package.
    pub fn tracks(&self, file: &Path) -> bool {
        self.paths.matches(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // The regression this guards: `version` used to be part of the derived identity, so a package
    // looked up after its version changed hashed and compared differently from the one that was
    // inserted. Nothing hit it yet only because no live code path mutates a version between
    // building a map and reading it back — a constraint nothing enforced.
    #[test]
    fn a_package_is_still_itself_after_its_version_changes() {
        let before = package("1.0.0");
        let mut after = before.clone();
        after.version = Version::parse("2.0.0").unwrap();

        assert_eq!(before, after);

        let map: HashMap<Package, &str> = HashMap::from([(before, "attributed commits")]);
        assert_eq!(map.get(&after), Some(&"attributed commits"));
    }

    #[test]
    fn packages_from_different_manifests_are_different_packages() {
        let a = package("1.0.0");
        let mut b = package("1.0.0");
        b.manifest = PathBuf::from("crate-b/Cargo.toml");

        assert_ne!(a, b);
    }

    #[test]
    fn a_package_tracks_its_own_directory_and_nothing_beside_it() {
        let p = package("1.0.0");

        assert!(p.tracks(Path::new("crate-a/src/lib.rs")));
        assert!(p.tracks(Path::new("crate-a/Cargo.toml")));
        assert!(!p.tracks(Path::new("crate-b/src/lib.rs")));
        assert!(!p.tracks(Path::new("README.md")));
    }

    // A string prefix would match here; `Path::starts_with` compares components, so it doesn't.
    #[test]
    fn a_sibling_sharing_a_name_prefix_is_not_tracked() {
        let p = package("1.0.0");

        assert!(!p.tracks(Path::new("crate-abc/src/lib.rs")));
    }

    #[test]
    fn a_root_package_tracks_the_whole_repo() {
        let root = Package::new(
            ".".to_string(),
            "root".to_string(),
            Version::parse("1.0.0").unwrap(),
            PathBuf::from("Cargo.toml"),
        );

        assert!(root.tracks(Path::new("src/lib.rs")));
        assert!(root.tracks(Path::new("Cargo.toml")));
        assert!(root.tracks(Path::new("crates/a/src/lib.rs")));
    }

    // The case a bare list of tracked prefixes cannot express: "the root package, minus the
    // members nested inside it".
    #[test]
    fn an_excluded_path_wins_over_the_tracked_path_containing_it() {
        let mut root = Package::new(
            ".".to_string(),
            "root".to_string(),
            Version::parse("1.0.0").unwrap(),
            PathBuf::from("Cargo.toml"),
        );
        root.paths.exclude("crates/a");

        assert!(!root.tracks(Path::new("crates/a/src/lib.rs")));
        assert!(root.tracks(Path::new("src/lib.rs")));
        assert!(root.tracks(Path::new("crates/b/src/lib.rs")));
    }

    // The editorial case: a subdirectory of the package's *own* tree that shouldn't trigger a
    // release. Same mechanism as the structural exclude above, different source.
    #[test]
    fn an_excluded_subdirectory_of_the_package_itself_is_not_tracked() {
        let mut p = package("1.0.0");
        p.paths.exclude("crate-a/benches");

        assert!(!p.tracks(Path::new("crate-a/benches/throughput.rs")));
        assert!(p.tracks(Path::new("crate-a/src/lib.rs")));
    }

    // The rule that "an exclude always wins" cannot express: carve a directory out, then put one
    // subdirectory of it back.
    #[test]
    fn a_tracked_path_nested_inside_an_excluded_one_wins_by_being_more_specific() {
        let mut p = package("1.0.0");
        p.paths.exclude("crate-a/tests");
        p.paths.track("crate-a/tests/compat");

        assert!(!p.tracks(Path::new("crate-a/tests/unit.rs")));
        assert!(p.tracks(Path::new("crate-a/tests/compat/v1.rs")));
        assert!(p.tracks(Path::new("crate-a/src/lib.rs")));
    }

    // Specificity is what decides, not which list a prefix came from, so alternating track and
    // exclude down a single chain keeps working at any depth.
    #[test]
    fn specificity_wins_at_every_depth_not_just_one_level() {
        let mut root = Package::new(
            ".".to_string(),
            "root".to_string(),
            Version::parse("1.0.0").unwrap(),
            PathBuf::from("Cargo.toml"),
        );
        root.paths.exclude("docs");
        root.paths.track("docs/api");
        root.paths.exclude("docs/api/scratch");

        assert!(root.tracks(Path::new("src/lib.rs")));
        assert!(!root.tracks(Path::new("docs/notes.md")));
        assert!(root.tracks(Path::new("docs/api/openapi.yaml")));
        assert!(!root.tracks(Path::new("docs/api/scratch/wip.md")));
    }

    // A contradictory instruction resolves to the narrower reading: a missing release is
    // recoverable, a spurious one is already published.
    #[test]
    fn a_path_both_tracked_and_excluded_is_excluded() {
        let mut p = package("1.0.0");
        p.paths.exclude("crate-a/benches");
        p.paths.track("crate-a/benches");

        assert!(!p.tracks(Path::new("crate-a/benches/throughput.rs")));
    }

    // An empty prefix matches everything, so accepting one would silently freeze the package at
    // its current version forever.
    #[test]
    fn an_empty_exclude_is_refused_rather_than_excluding_everything() {
        let mut p = package("1.0.0");
        p.paths.exclude("");

        assert!(p.tracks(Path::new("crate-a/src/lib.rs")));
    }

    // The mirror hazard: an empty track would hand this package every file in the repo, including
    // its siblings', and at specificity 0 it would lose to nothing at all.
    #[test]
    fn an_empty_track_is_refused_rather_than_tracking_the_whole_repo() {
        let mut p = package("1.0.0");
        p.paths.track("");

        assert!(!p.tracks(Path::new("crate-b/src/lib.rs")));
    }

    fn package(version: &str) -> Package {
        Package::new(
            "crate-a".to_string(),
            "crate-a".to_string(),
            Version::parse(version).unwrap(),
            PathBuf::from("crate-a/Cargo.toml"),
        )
    }
}
