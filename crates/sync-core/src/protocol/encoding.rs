//! Message encoding detection.
//!
//! The P2P protocol uses multiple encodings:
//! - **JSON**: Handshake and SWIM gossip messages (human-readable, cross-platform)
//! - **Bincode**: Sync messages (binary, efficient for CRDT data)
//!
//! This module provides utilities for detecting which encoding a message uses.

/// Type of message based on encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    /// JSON message (handshake or SWIM)
    Json,
    /// Bincode message (sync data)
    Bincode,
    /// Empty message
    Empty,
}

/// Detect the message type from raw bytes.
///
/// JSON messages start with `{` (object) or `[` (array).
/// Everything else is treated as Bincode.
pub fn detect_message_type(data: &[u8]) -> MessageType {
    match data.first() {
        Some(b'{') | Some(b'[') => MessageType::Json,
        Some(_) => MessageType::Bincode,
        None => MessageType::Empty,
    }
}

/// Check if data is likely a JSON message.
pub fn is_likely_json(data: &[u8]) -> bool {
    matches!(detect_message_type(data), MessageType::Json)
}

/// Check if data is likely a Bincode message.
pub fn is_likely_bincode(data: &[u8]) -> bool {
    matches!(detect_message_type(data), MessageType::Bincode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_json_object() {
        let data = br#"{"type":"handshake","version":1}"#;
        assert_eq!(detect_message_type(data), MessageType::Json);
        assert!(is_likely_json(data));
        assert!(!is_likely_bincode(data));
    }

    #[test]
    fn test_detect_json_array() {
        let data = br#"[1, 2, 3]"#;
        assert_eq!(detect_message_type(data), MessageType::Json);
    }

    #[test]
    fn test_detect_bincode() {
        // Bincode messages typically start with length prefix or variant tag
        let data = vec![0x00, 0x01, 0x02, 0x03];
        assert_eq!(detect_message_type(&data), MessageType::Bincode);
        assert!(is_likely_bincode(&data));
        assert!(!is_likely_json(&data));
    }

    #[test]
    fn test_detect_empty() {
        assert_eq!(detect_message_type(&[]), MessageType::Empty);
    }

    #[test]
    fn test_bincode_sync_message() {
        // Simulate a bincode-encoded sync message (variant 0)
        let data = vec![0x00, 0x00, 0x00, 0x00];
        assert_eq!(detect_message_type(&data), MessageType::Bincode);
    }

    #[test]
    fn test_whitespace_is_bincode() {
        // Leading whitespace is not typical JSON from our serializers
        // Treat as bincode (will fail to parse, but that's the caller's job)
        let data = b" {\"test\": 1}";
        assert_eq!(detect_message_type(data), MessageType::Bincode);
    }
}
