//! The app's version: where it comes from and how two versions compare.

/// The `version` of Cargo.toml. It is the only place the number is written.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// The short git commit of the build, or "unknown". Set by build.rs.
pub const COMMIT: &str = env!("FLINT_GIT_COMMIT");

/// "0.1.0 (abc1234)", as the About page shows it.
pub fn label() -> String {
    format!("{CURRENT} ({COMMIT})")
}

/// `MAJOR.MINOR.PATCH`, digits only. Ordered the way versions are.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    major: u16,
    minor: u16,
    patch: u16,
}

impl Version {
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('.');
        let version = Self {
            major: number(parts.next()?)?,
            minor: number(parts.next()?)?,
            patch: number(parts.next()?)?,
        };
        parts.next().is_none().then_some(version)
    }
}

/// One to four digits. `str::parse` alone would also take "+1".
fn number(part: &str) -> Option<u16> {
    let is_digits = (1..=4).contains(&part.len()) && part.bytes().all(|byte| byte.is_ascii_digit());
    if !is_digits {
        return None;
    }
    part.parse().ok()
}

/// Whether `offered` is a later version than `current`. Text that is not a
/// version on either side means no: a broken answer must not offer an update.
pub fn is_newer(offered: &str, current: &str) -> bool {
    match (Version::parse(offered), Version::parse(current)) {
        (Some(offered), Some(current)) => offered > current,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(major: u16, minor: u16, patch: u16) -> Option<Version> {
        Some(Version {
            major,
            minor,
            patch,
        })
    }

    #[test]
    fn three_numbers_parse() {
        assert_eq!(Version::parse("0.1.0"), version(0, 1, 0));
        assert_eq!(Version::parse("12.340.9999"), version(12, 340, 9999));
    }

    #[test]
    fn leading_zeros_are_read_as_numbers() {
        assert_eq!(Version::parse("0.01.007"), version(0, 1, 7));
        assert!(!is_newer("0.01.0", "0.1.0"));
    }

    #[test]
    fn anything_else_is_not_a_version() {
        let garbage = [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "1.2.",
            ".1.2",
            "1..2",
            "v1.2.3",
            "1.2.3-beta",
            "1.2.3+4",
            "1.2.x",
            " 1.2.3",
            "1.2.3 ",
            "+1.2.3",
            "-1.2.3",
            "1.2.12345",
            "１.2.3",
            "1,2,3",
        ];
        for text in garbage {
            assert_eq!(Version::parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn numbers_compare_as_numbers_and_not_as_text() {
        assert!(Version::parse("0.10.0") > Version::parse("0.9.9"));
        assert!(Version::parse("1.0.0") > Version::parse("0.99.99"));
        assert!(Version::parse("0.1.1") > Version::parse("0.1.0"));
        assert!(Version::parse("0.1.0") == Version::parse("0.1.0"));
    }

    #[test]
    fn only_a_later_version_is_newer() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
    }

    #[test]
    fn a_broken_version_on_either_side_is_never_newer() {
        assert!(!is_newer("9.9.9.9", "0.1.0"));
        assert!(!is_newer("latest", "0.1.0"));
        assert!(!is_newer("", "0.1.0"));
        assert!(!is_newer("9.9.9", "dev"));
    }

    #[test]
    fn the_app_itself_has_a_version_that_parses() {
        assert!(Version::parse(CURRENT).is_some(), "{CURRENT}");
        assert!(!COMMIT.is_empty());
    }
}
