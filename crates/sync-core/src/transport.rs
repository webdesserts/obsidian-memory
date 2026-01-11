//! SyncTransport trait for peer-to-peer networking.
//!
//! Implementations:
//! - LAN: mDNS discovery + WebSocket (desktop only)
//! - WebRTC: Signaling server + WebRTC DataChannel (all platforms)

use async_trait::async_trait;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("Connection failed: {0}")]
    ConnectionFailed(String),

    #[error("Peer not found: {0}")]
    PeerNotFound(String),

    #[error("Send failed: {0}")]
    SendFailed(String),

    #[error("Receive failed: {0}")]
    ReceiveFailed(String),

    #[error("Transport error: {0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, TransportError>;

/// Information about a peer
#[derive(Debug, Clone)]
pub struct PeerInfo {
    /// Unique peer identifier
    pub id: String,
    /// Human-readable name (if available)
    pub name: Option<String>,
    /// Connection address (IP:port, URL, etc.)
    pub address: String,
}

/// An active connection to a peer
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait PeerConnection: Send + Sync {
    /// Get the connected peer's ID
    fn peer_id(&self) -> &str;

    /// Send data to the peer
    async fn send(&self, data: &[u8]) -> Result<()>;

    /// Receive data from the peer (blocks until data available)
    async fn recv(&self) -> Result<Vec<u8>>;

    /// Close the connection
    async fn close(&self) -> Result<()>;
}

/// Transport layer for P2P sync
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait SyncTransport: Send + Sync {
    /// Get our peer ID
    fn peer_id(&self) -> &str;

    /// Discover available peers
    async fn discover_peers(&self) -> Result<Vec<PeerInfo>>;

    /// Connect to a peer
    async fn connect(&self, peer: &PeerInfo) -> Result<Box<dyn PeerConnection>>;

    /// Accept incoming connections (returns stream of connections)
    /// Note: This is a simplified API. Real implementations would use async streams.
    async fn accept(&self) -> Result<Box<dyn PeerConnection>>;
}
