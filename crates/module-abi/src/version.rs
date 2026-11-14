//! ABI and engine API versions and the one rule that accepts them.

use std::fmt;

/// The engine API this build implements (point semantics and wire encodings).
pub const ENGINE_API_VERSION: Version = Version { major: 1, minor: 0 };

/// A `major.minor` version as the descriptor spells it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Version {
    /// Incompatible changes.
    pub major: u16,
    /// Additive changes.
    pub minor: u16,
}

impl Version {
    /// Parses `major.minor` with no sign, no leading zero and no extra part.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (major, minor) = text.split_once('.')?;
        Some(Self {
            major: number(major)?,
            minor: number(minor)?,
        })
    }

    /// The packed `(major << 16) | minor` form `hs_abi_version` returns.
    #[must_use]
    pub const fn packed(self) -> u32 {
        pack_abi(self.major, self.minor)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

fn number(text: &str) -> Option<u16> {
    let plain = !text.is_empty() && text.len() <= 5 && text.bytes().all(|b| b.is_ascii_digit());
    if !plain || (text.len() > 1 && text.starts_with('0')) {
        return None;
    }
    text.parse().ok()
}

/// Packs an ABI version as the header does.
#[must_use]
pub const fn pack_abi(major: u16, minor: u16) -> u32 {
    ((major as u32) << 16) | minor as u32
}

/// The version rule of section 3: the major must be equal and the module's
/// minor must not exceed the host's. A newer minor than the host is refused; an
/// older one is accepted.
#[must_use]
pub const fn abi_accepted(module: u32, host: u32) -> bool {
    module >> 16 == host >> 16 && (module & 0xFFFF) <= (host & 0xFFFF)
}

impl Version {
    /// The same rule for engine API versions.
    #[must_use]
    pub const fn accepted_by(self, host: Self) -> bool {
        abi_accepted(self.packed(), host.packed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_accepts_equal_and_older_minor_only() {
        let host = pack_abi(1, 3);
        assert!(abi_accepted(pack_abi(1, 3), host));
        assert!(abi_accepted(pack_abi(1, 0), host));
        assert!(!abi_accepted(pack_abi(1, 4), host));
        assert!(!abi_accepted(pack_abi(2, 0), host));
        assert!(!abi_accepted(pack_abi(0, 0), host));
    }

    #[test]
    fn parse_is_strict() {
        assert_eq!(Version::parse("1.0"), Some(Version { major: 1, minor: 0 }));
        assert_eq!(
            Version::parse("65535.65535"),
            Some(Version {
                major: 65535,
                minor: 65535
            })
        );
        for bad in [
            "", "1", "1.", ".1", "01.0", "1.00", "1.0.0", "+1.0", "-1.0", "65536.0", "1.0 ",
            "1. 0", "a.b",
        ] {
            assert_eq!(Version::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn display_round_trips() {
        assert_eq!(
            Version::parse(
                &Version {
                    major: 7,
                    minor: 12
                }
                .to_string()
            ),
            Some(Version {
                major: 7,
                minor: 12
            })
        );
    }

    #[test]
    fn engine_api_uses_the_same_rule() {
        let host = Version { major: 1, minor: 0 };
        assert!(Version { major: 1, minor: 0 }.accepted_by(host));
        assert!(!Version { major: 1, minor: 1 }.accepted_by(host));
        assert!(!Version { major: 2, minor: 0 }.accepted_by(host));
    }
}
