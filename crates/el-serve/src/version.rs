//! Electrum protocol version parsing and negotiation.
//!
//! Electrum versions are dotted numeric strings (`1.4`, `1.4.2`, `1.7`).  The
//! server speaks every version between `SERVER_MIN` and `SERVER_MAX`; the
//! negotiated version is the highest version both sides support.

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

    /// The highest version both the client range and the server range support.
    pub fn negotiate(
        client_min: Self,
        client_max: Self,
        server_min: Self,
        server_max: Self,
    ) -> Option<Self> {
        if client_max < client_min {
            return None;
        }
        let negotiated = client_max.min(server_max);
        if negotiated < client_min.max(server_min) {
            None
        } else {
            Some(negotiated)
        }
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
/// 1.4.2 adds `blockchain.scripthash.unsubscribe` (implemented).  1.4.3 only
/// adds a Namecoin method, so it is deliberately not advertised.
pub const SERVER_MIN: ProtocolVersion = ProtocolVersion::new(1, 4, 0);
pub const SERVER_MAX: ProtocolVersion = ProtocolVersion::new(1, 4, 2);

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
    fn negotiation_picks_the_highest_common_version() {
        let min = ProtocolVersion::new(1, 4, 0);
        let max = ProtocolVersion::new(1, 7, 0);
        assert_eq!(
            ProtocolVersion::negotiate(min, max, SERVER_MIN, SERVER_MAX),
            Some(SERVER_MAX)
        );
        assert_eq!(
            ProtocolVersion::negotiate(
                ProtocolVersion::new(1, 4, 1),
                ProtocolVersion::new(1, 4, 1),
                SERVER_MIN,
                SERVER_MAX
            ),
            Some(ProtocolVersion::new(1, 4, 1))
        );
        assert_eq!(
            ProtocolVersion::negotiate(
                ProtocolVersion::new(1, 5, 0),
                ProtocolVersion::new(1, 7, 0),
                SERVER_MIN,
                SERVER_MAX
            ),
            None
        );
        assert_eq!(
            ProtocolVersion::negotiate(
                ProtocolVersion::new(1, 4, 3),
                ProtocolVersion::new(1, 4, 3),
                SERVER_MIN,
                SERVER_MAX
            ),
            None
        );
    }
}
