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

    /// Applies this bump to an arbitrary version. Unlike reapplying the same `Bump` repeatedly to
    /// its own output, this is only safe to call once per "required severity" decision: `Major`
    /// and `Minor` zero out the lower fields so reapplying the same level is harmless, but `Patch`
    /// just increments by one every time, so applying it to an already-patched version double
    /// bumps instead of recognizing that version already covers it. Callers must always apply to
    /// the version *before* any not-yet-released bump on this branch (the baseline), not to
    /// whatever's currently staged on disk.
    #[must_use]
    pub fn apply_to(self, version: &Version) -> Version {
        let mut new = version.clone();
        match self {
            Self::Patch => new.patch += 1,
            Self::Minor => {
                new.minor += 1;
                new.patch = 0;
            }
            Self::Major => {
                new.major += 1;
                new.minor = 0;
                new.patch = 0;
            }
        }
        new
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_to_increments_the_right_field_and_zeroes_lower_ones() {
        let v = Version::parse("1.2.5").unwrap();
        assert_eq!(Bump::Patch.apply_to(&v), Version::parse("1.2.6").unwrap());
        assert_eq!(Bump::Minor.apply_to(&v), Version::parse("1.3.0").unwrap());
        assert_eq!(Bump::Major.apply_to(&v), Version::parse("2.0.0").unwrap());
    }
}
