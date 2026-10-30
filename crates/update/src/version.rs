//! Stoop's version numbers, and which of two is newer.
//!
//! A release is `MAJOR.MINOR.PATCH` (tag `v0.3.0`); a hotfix built from a
//! push to main adds the workflow's run number as a fourth part (tag
//! `hotfix-0.3.0.68`), see `.github/workflows/release.yml`. A hotfix is
//! always built after the release it extends, so it sorts above it, and
//! the next release sorts above every hotfix of the one before:
//!
//! ```text
//! 0.3.0  <  0.3.0.1  <  0.3.0.68  <  0.3.1
//! ```
//!
//! Comparing the parts as numbers, with a missing fourth part read as 0,
//! gives exactly that order.

use std::cmp::Ordering;
use std::fmt;

/// A version of Stoop: three or four numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Version {
    /// `MAJOR.MINOR.PATCH`.
    pub release: [u64; 3],
    /// The hotfix's run number, for a hotfix build; `None` for a release.
    pub hotfix: Option<u64>,
}

impl Version {
    /// Reads a version from text: `0.3.0`, `0.3.0.68`, or a release tag
    /// (`v0.3.0`, `hotfix-0.3.0.68`). Anything else is `None`.
    pub fn parse(text: &str) -> Option<Version> {
        let text = text.trim();
        // Tags carry a prefix the number does not.
        let text = text.strip_prefix("hotfix-").or_else(|| text.strip_prefix('v')).unwrap_or(text);
        let mut parts = text.split('.').map(|p| if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) { None } else { p.parse::<u64>().ok() });
        let release = [parts.next()??, parts.next()??, parts.next()??];
        let hotfix = match parts.next() {
            None => None,
            Some(n) => Some(n?),
        };
        // A fifth part is not a version Stoop makes.
        if parts.next().is_some() {
            return None;
        }
        Some(Version { release, hotfix })
    }

    /// The version this copy of the app was built as.
    ///
    /// The release workflow sets `STOOP_VERSION` to the full number,
    /// hotfix part included, while it builds; Cargo's own version only
    /// ever holds the first three parts, since a crate version has no room
    /// for a fourth. A local build has no `STOOP_VERSION` and reports the
    /// crate's version.
    pub fn current() -> Version {
        option_env!("STOOP_VERSION")
            .and_then(Version::parse)
            .or_else(|| Version::parse(env!("CARGO_PKG_VERSION")))
            .unwrap_or(Version { release: [0, 0, 0], hotfix: None })
    }

    /// The parts as four numbers, a release's hotfix part being 0, which is
    /// what [`Ord`] compares.
    fn key(&self) -> [u64; 4] {
        let [a, b, c] = self.release;
        [a, b, c, self.hotfix.unwrap_or(0)]
    }
}

impl Ord for Version {
    /// Newer is greater; see the module documentation for the order.
    fn cmp(&self, other: &Self) -> Ordering {
        self.key().cmp(&other.key())
    }
}

impl PartialOrd for Version {
    /// The same order as [`Ord`].
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    /// As people read it: `0.3.0`, or `0.3.0.68` for a hotfix.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c] = self.release;
        write!(f, "{a}.{b}.{c}")?;
        if let Some(n) = self.hotfix {
            write!(f, ".{n}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shorthand for a version that must parse.
    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn reads_numbers_and_tags() {
        assert_eq!(v("0.3.0"), Version { release: [0, 3, 0], hotfix: None });
        assert_eq!(v("v1.2.3"), Version { release: [1, 2, 3], hotfix: None });
        assert_eq!(v("hotfix-0.3.0.68"), Version { release: [0, 3, 0], hotfix: Some(68) });
        assert_eq!(v(" 0.3.0.68\n"), v("0.3.0.68"));
    }

    #[test]
    fn refuses_what_is_not_a_version() {
        for bad in ["", "0.3", "0.3.x", "0..3", "1.2.3.4.5", "v", "0.3.0-rc1", "+1.2.3", "test-0.1.0"] {
            assert_eq!(Version::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn hotfixes_sort_between_releases() {
        assert!(v("0.3.0") < v("0.3.0.1"));
        assert!(v("0.3.0.9") < v("0.3.0.68"));
        assert!(v("0.3.0.68") < v("0.3.1"));
        assert!(v("0.2.0.64") < v("0.3.0"));
        assert!(v("0.9.9") < v("0.10.0"));
        assert_eq!(v("0.3.0").cmp(&v("0.3.0")), Ordering::Equal);
    }

    #[test]
    fn shows_as_written() {
        assert_eq!(v("v0.3.0").to_string(), "0.3.0");
        assert_eq!(v("hotfix-0.3.0.68").to_string(), "0.3.0.68");
    }

    #[test]
    fn knows_its_own_version() {
        // A test build has no STOOP_VERSION unless the release workflow set
        // one; either way it is a version.
        let current = Version::current();
        assert!(current >= v("0.0.0"));
    }
}
