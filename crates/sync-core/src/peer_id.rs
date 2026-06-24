//! VaultId: stable author identity for a vault's Loro CRDT operations.
//!
//! `PeerId` (the network-client identity) and the `KeyStorage` trait live in
//! `p2p-core`; this module re-exports them so existing `sync_core::PeerId` /
//! `sync_core::peer_id::*` call sites keep resolving. `VaultId` — the
//! vault-semantic type — stays here because it is sync-core's, not a generic
//! networking concept.

use std::fmt::{self, Display, Formatter};
use std::str::FromStr;

pub use p2p_core::PeerId;
pub use p2p_core::peer_id::PeerIdError;

/// A stable author identity for a vault, used as the Loro peer ID for CRDT operations.
///
/// Unlike `PeerId` (which identifies a network client), `VaultId` identifies the vault
/// itself and is shared across all clients accessing the same vault copy. Generated once
/// and persisted in `.sync/metadata.toml`.
///
/// Uses a u64 internally (Loro-native format) displayed as a 16-character hex string.
///
/// # Examples
/// ```
/// use sync_core::VaultId;
///
/// let vault_id = VaultId::generate();
/// println!("{}", vault_id);  // "a1b2c3d4e5f67890"
///
/// let parsed: VaultId = "a1b2c3d4e5f67890".parse().unwrap();
/// assert_eq!(parsed.as_u64(), 0xa1b2c3d4e5f67890);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VaultId(u64);

impl VaultId {
    /// Generate a new random vault ID.
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

impl Display for VaultId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

impl FromStr for VaultId {
    type Err = PeerIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // 16 hex chars only — VaultId has no legacy UUID format
        if s.len() == 16 && s.chars().all(|c| c.is_ascii_hexdigit()) {
            let id = u64::from_str_radix(&s.to_ascii_lowercase(), 16)
                .map_err(|e| PeerIdError::InvalidHex(e.to_string()))?;
            return Ok(Self(id));
        }

        Err(PeerIdError::InvalidFormat)
    }
}

impl From<u64> for VaultId {
    fn from(id: u64) -> Self {
        Self(id)
    }
}

impl From<VaultId> for u64 {
    fn from(vault_id: VaultId) -> u64 {
        vault_id.0
    }
}

impl serde::Serialize for VaultId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for VaultId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vault_id_generate_not_zero() {
        for _ in 0..1000 {
            assert_ne!(VaultId::generate().as_u64(), 0);
        }
    }

    #[test]
    fn test_vault_id_display_hex() {
        let id = VaultId(0xa1b2c3d4e5f67890);
        assert_eq!(id.to_string(), "a1b2c3d4e5f67890");
    }

    #[test]
    fn test_vault_id_display_zero_padded() {
        let id = VaultId(0xff);
        assert_eq!(id.to_string(), "00000000000000ff");
    }

    #[test]
    fn test_vault_id_parse_hex() {
        let id: VaultId = "a1b2c3d4e5f67890".parse().unwrap();
        assert_eq!(id.as_u64(), 0xa1b2c3d4e5f67890);
    }

    #[test]
    fn test_vault_id_roundtrip() {
        let original = VaultId::generate();
        let serialized = original.to_string();
        let parsed: VaultId = serialized.parse().unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn test_vault_id_rejects_uuid() {
        // VaultId has no legacy UUID support
        assert!(
            "550e8400-e29b-41d4-a716-446655440000"
                .parse::<VaultId>()
                .is_err()
        );
    }

    #[test]
    fn test_vault_id_serde_roundtrip() {
        let original = VaultId::generate();
        let json = serde_json::to_string(&original).unwrap();
        let parsed: VaultId = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }
}
