use cargo_metadata::semver::Version;

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct Package {
    /// Workspace-relative directory containing the package's `Cargo.toml`.
    pub path: String,
    /// The package's actual `[package] name` from its `Cargo.toml`.
    pub name: String,
    pub version: Version,
}

impl Package {
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
}
