/// Generate a deterministic 32-byte key seed from a small integer.
///
/// Used by relay_integration and daemon_integration tests to build iroh nodes
/// with repeatable identities. Each unique `n` produces a distinct key.
pub fn seed(n: u8) -> [u8; 32] {
    [n; 32]
}
