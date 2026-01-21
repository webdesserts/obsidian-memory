//! PeerId: Unique identifier for a peer/device in the sync network.
//!
//! Wraps a u64 internally (for Loro compatibility) but displays as
//! a 16-character hex string for human readability.

use std::fmt::{self, Display, Formatter};
use std::str::FromStr;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PeerIdError {
    #[error("Invalid peer ID format: expected 16 hex chars or UUID")]
    InvalidFormat,
    #[error("Invalid hex: {0}")]
    InvalidHex(#[from] std::num::ParseIntError),
}

/// A unique identifier for a peer/device in the sync network.
///
/// Wraps a u64 internally (for Loro compatibility) but displays as
/// a 16-character hex string for human readability.
///
/// # Examples
/// ```
/// use sync_core::PeerId;
///
/// let peer_id = PeerId::generate();
/// println!("{}", peer_id);  // "a1b2c3d4e5f67890"
///
/// let parsed: PeerId = "a1b2c3d4e5f67890".parse().unwrap();
/// assert_eq!(parsed.as_u64(), 0xa1b2c3d4e5f67890);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeerId(u64);

impl PeerId {
    /// Generate a new random peer ID.
    ///
    /// Uses cryptographically secure randomness. Never returns zero.
    pub fn generate() -> Self {
        use rand::Rng;
        loop {
            let id: u64 = rand::rng().random();
            if id != 0 {
                return Self(id);
            }
        }
    }

    /// Get the underlying u64 value (for Loro API).
    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

impl Display for PeerId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

impl FromStr for PeerId {
    type Err = PeerIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // New format: 16 hex chars
        if s.len() == 16 && s.chars().all(|c| c.is_ascii_hexdigit()) {
            let id =
                u64::from_str_radix(&s.to_ascii_lowercase(), 16).map_err(PeerIdError::InvalidHex)?;
            return Ok(Self(id));
        }

        // Legacy format: UUID (36 chars with dashes at positions 8, 13, 18, 23) → hash with FNV-1a
        // Lowercase for consistency (same UUID with different case → same hash)
        if s.len() == 36 {
            let bytes = s.as_bytes();
            if bytes[8] == b'-' && bytes[13] == b'-' && bytes[18] == b'-' && bytes[23] == b'-' {
                return Ok(Self(fnv1a_hash(&s.to_ascii_lowercase())));
            }
        }

        Err(PeerIdError::InvalidFormat)
    }
}

impl From<u64> for PeerId {
    fn from(id: u64) -> Self {
        Self(id)
    }
}

impl From<PeerId> for u64 {
    fn from(peer_id: PeerId) -> u64 {
        peer_id.0
    }
}

// Serialize as hex string for consistency in logs, errors, JSON
impl serde::Serialize for PeerId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for PeerId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// FNV-1a hash for legacy UUID migration.
/// Stable across Rust versions (unlike DefaultHasher).
fn fnv1a_hash(s: &str) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET;
    for byte in s.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_display_hex() {
        let peer_id = PeerId(0xa1b2c3d4e5f67890);
        assert_eq!(peer_id.to_string(), "a1b2c3d4e5f67890");
    }

    #[test]
    fn test_display_zero_padded() {
        let peer_id = PeerId(0xff);
        assert_eq!(peer_id.to_string(), "00000000000000ff");
    }

    #[test]
    fn test_parse_hex() {
        let peer_id: PeerId = "a1b2c3d4e5f67890".parse().unwrap();
        assert_eq!(peer_id.as_u64(), 0xa1b2c3d4e5f67890);
    }

    #[test]
    fn test_parse_uppercase_hex() {
        let peer_id: PeerId = "A1B2C3D4E5F67890".parse().unwrap();
        assert_eq!(peer_id.as_u64(), 0xa1b2c3d4e5f67890);
    }

    #[test]
    fn test_parse_legacy_uuid() {
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        let peer_id: PeerId = uuid.parse().unwrap();
        // Should produce consistent hash
        let peer_id2: PeerId = uuid.parse().unwrap();
        assert_eq!(peer_id, peer_id2);
    }

    #[test]
    fn test_roundtrip() {
        let original = PeerId::generate();
        let serialized = original.to_string();
        let parsed: PeerId = serialized.parse().unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn test_invalid_format() {
        assert!("too_short".parse::<PeerId>().is_err());
        assert!("not-a-valid-format-at-all".parse::<PeerId>().is_err());
        assert!("ghijklmnopqrstuv".parse::<PeerId>().is_err()); // non-hex
    }

    #[test]
    fn test_generate_not_zero() {
        // Generate many and ensure none are zero
        for _ in 0..1000 {
            assert_ne!(PeerId::generate().as_u64(), 0);
        }
    }

    #[test]
    fn test_parse_uuid_case_insensitive() {
        let lower = "550e8400-e29b-41d4-a716-446655440000";
        let upper = "550E8400-E29B-41D4-A716-446655440000";
        let mixed = "550e8400-E29B-41d4-A716-446655440000";

        let p1: PeerId = lower.parse().unwrap();
        let p2: PeerId = upper.parse().unwrap();
        let p3: PeerId = mixed.parse().unwrap();

        assert_eq!(p1, p2);
        assert_eq!(p2, p3);
    }

    #[test]
    fn test_reject_wrong_length() {
        assert!("a1b2c3d4e5f6789".parse::<PeerId>().is_err()); // 15 chars
        assert!("a1b2c3d4e5f678901".parse::<PeerId>().is_err()); // 17 chars
        assert!("".parse::<PeerId>().is_err()); // empty
    }

    #[test]
    fn test_reject_invalid_uuid() {
        // Wrong number of dashes
        assert!("550e8400e29b-41d4-a716-446655440000"
            .parse::<PeerId>()
            .is_err());
        // Wrong positions
        assert!("550e8400-e29b41d4-a716-4466-55440000"
            .parse::<PeerId>()
            .is_err());
    }

    #[test]
    fn test_serde_roundtrip() {
        let original = PeerId::generate();
        let json = serde_json::to_string(&original).unwrap();
        let parsed: PeerId = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }
}
