//! Electrum protocol version parsing and negotiation.
//!
//! Electrum versions are dotted numeric strings (`1.4`, `1.4.2`, `1.7`).  The
//! server keeps an explicit list of implemented versions and negotiates down to
//! the newest one at or below what the client asked for.  A client asking for a
//! future or skipped version (1.5, 1.6 before it is implemented) is not
//! disconnected; it is served the previous implemented version, and the
//! response tells it which version is in effect.

use std::fmt;

/// A parsed Electrum protocol version.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

impl ProtocolVersion {
    pub const fn new(major: u16, minor: u16, patch: u16) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Parse `a.b` or `a.b.c`.  A missing patch is zero.
    pub fn parse(value: &str) -> Result<Self, String> {
        let value = value.trim();
        if value.is_empty() {
            return Err("empty protocol version".to_string());
        }
        let parts: Vec<&str> = value.split('.').collect();
        if !(2..=3).contains(&parts.len()) {
            return Err(format!("invalid protocol version: {value}"));
        }
        let parse_part = |part: &str, what: &str| -> Result<u16, String> {
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!("invalid protocol version {what}: {part:?}"));
            }
            part.parse::<u16>()
                .map_err(|_| format!("protocol version {what} out of range: {part}"))
        };
        let major = parse_part(parts[0], "major")?;
        let minor = parse_part(parts[1], "minor")?;
        let patch = if parts.len() == 3 {
            parse_part(parts[2], "patch")?
        } else {
            0
        };
        Ok(Self::new(major, minor, patch))
    }

    /// The newest implemented version that does not exceed `client_max`.
    ///
    /// This is deliberately lenient: a client asking for an unimplemented
    /// future version (or a gap such as 1.5) falls back to the previous
    /// available version instead of losing the connection.  The caller should
    /// only reject when no version at or below `client_max` exists.
    pub fn highest_available(client_max: Self, available: &[Self]) -> Option<Self> {
        available
            .iter()
            .copied()
            .filter(|version| *version <= client_max)
            .max()
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.patch == 0 {
            write!(f, "{}.{}", self.major, self.minor)
        } else {
            write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
        }
    }
}

/// Lowest and highest protocol versions this build can serve.
///
/// 1.4.1 only changes AuxPoW header truncation (irrelevant on Bitcoin) and
/// 1.4.2 adds `blockchain.scripthash.unsubscribe` (implemented).  1.4.3 only
/// adds a Namecoin method, so it is deliberately not listed.
pub const SERVER_MIN: ProtocolVersion = ProtocolVersion::new(1, 4, 0);
pub const SERVER_MAX: ProtocolVersion = ProtocolVersion::new(1, 4, 2);

/// Implemented protocol versions, sorted ascending.  Add a version here only
/// after every method it documents is implemented and tested.
pub const SERVER_VERSIONS: &[ProtocolVersion] =
    &[SERVER_MIN, ProtocolVersion::new(1, 4, 1), SERVER_MAX];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_two_and_three_component_versions() {
        assert_eq!(
            ProtocolVersion::parse("1.4"),
            Ok(ProtocolVersion::new(1, 4, 0))
        );
        assert_eq!(
            ProtocolVersion::parse("1.4.2"),
            Ok(ProtocolVersion::new(1, 4, 2))
        );
        assert_eq!(
            ProtocolVersion::parse(" 1.7 "),
            Ok(ProtocolVersion::new(1, 7, 0))
        );
    }

    #[test]
    fn rejects_malformed_versions() {
        for value in ["", "1", "1.", ".4", "1.4.2.1", "1.x", "-1.4", "1..2"] {
            assert!(
                ProtocolVersion::parse(value).is_err(),
                "{value:?} must be rejected"
            );
        }
    }

    #[test]
    fn display_omits_zero_patch() {
        assert_eq!(ProtocolVersion::new(1, 4, 0).to_string(), "1.4");
        assert_eq!(ProtocolVersion::new(1, 4, 2).to_string(), "1.4.2");
    }

    #[test]
    fn selects_newest_version_at_or_below_client_max() {
        let version = ProtocolVersion::new;
        assert_eq!(
            ProtocolVersion::highest_available(version(1, 4, 0), SERVER_VERSIONS),
            Some(version(1, 4, 0))
        );
        assert_eq!(
            ProtocolVersion::highest_available(version(1, 4, 1), SERVER_VERSIONS),
            Some(version(1, 4, 1))
        );
        assert_eq!(
            ProtocolVersion::highest_available(version(1, 4, 2), SERVER_VERSIONS),
            Some(version(1, 4, 2))
        );
        // 1.4.3 is Namecoin-only, so it is not in our list.
        assert_eq!(
            ProtocolVersion::highest_available(version(1, 4, 3), SERVER_VERSIONS),
            Some(version(1, 4, 2))
        );
        // 1.5 was skipped and 1.6 is not implemented yet: previous available.
        assert_eq!(
            ProtocolVersion::highest_available(version(1, 5, 0), SERVER_VERSIONS),
            Some(version(1, 4, 2))
        );
        assert_eq!(
            ProtocolVersion::highest_available(version(1, 6, 0), SERVER_VERSIONS),
            Some(version(1, 4, 2))
        );
        assert_eq!(
            ProtocolVersion::highest_available(version(1, 7, 0), SERVER_VERSIONS),
            Some(version(1, 4, 2))
        );
        assert_eq!(
            ProtocolVersion::highest_available(version(1, 3, 9), SERVER_VERSIONS),
            None
        );
    }

    #[test]
    fn future_versions_fall_back_to_the_previous_available_one() {
        let version = ProtocolVersion::new;
        let mut available = SERVER_VERSIONS.to_vec();
        available.push(version(1, 6, 0));
        assert_eq!(
            ProtocolVersion::highest_available(version(1, 4, 3), &available),
            Some(version(1, 4, 2)),
            "a version between available ones must not pick the newer protocol"
        );
        assert_eq!(
            ProtocolVersion::highest_available(version(1, 5, 0), &available),
            Some(version(1, 4, 2))
        );
        assert_eq!(
            ProtocolVersion::highest_available(version(1, 6, 0), &available),
            Some(version(1, 6, 0))
        );
        assert_eq!(
            ProtocolVersion::highest_available(version(1, 7, 0), &available),
            Some(version(1, 6, 0))
        );
    }

    #[test]
    fn available_versions_are_sorted() {
        assert!(SERVER_VERSIONS.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(SERVER_VERSIONS.first(), Some(&SERVER_MIN));
        assert_eq!(SERVER_VERSIONS.last(), Some(&SERVER_MAX));
    }
}
