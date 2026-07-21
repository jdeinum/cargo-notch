use crate::package::Package;
use cargo_metadata::semver::Version;

/// A semver bump level. Ordered so `max()` picks the biggest bump
/// (`Patch < Minor < Major`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Bump {
    Patch,
    Minor,
    Major,
}

impl Bump {
    pub const ALL: [Self; 3] = [Self::Patch, Self::Minor, Self::Major];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Patch => "patch",
            Self::Minor => "minor",
            Self::Major => "major",
        }
    }

    #[must_use]
    pub fn apply(self, package: &Package) -> Version {
        match self {
            Self::Patch => package.bump_patch(),
            Self::Minor => package.bump_minor(),
            Self::Major => package.bump_major(),
        }
    }
}
