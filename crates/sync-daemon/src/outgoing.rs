//! Outgoing peer connections.
//!
//! Manages connections we initiate to remote peers, including:
//! - Connection establishment with handshake
//! - Automatic reconnection with exponential backoff
//! - State tracking (connecting, connected, reconnecting)

use crate::connection::ConnectionEvent;
use crate::message::HandshakeMessage;
use anyhow::Result;
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Error as WsError, Message},
    MaybeTlsStream, WebSocketStream,
};
use tracing::{debug, error, info, warn};

/// Maximum message size for outgoing connections (50MB).
pub const MAX_MESSAGE_SIZE: usize = 50 * 1024 * 1024;

/// State of an outgoing connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutgoingState {
    /// Attempting to connect
    Connecting,
    /// Connected and handshaking
    Handshaking,
    /// Fully connected
    Connected,
    /// Disconnected, waiting to reconnect
    Reconnecting,
    /// Permanently closed (no reconnect)
    Closed,
}

/// Configuration for reconnection behavior.
#[derive(Debug, Clone)]
pub struct ReconnectConfig {
    /// Initial delay before first reconnect attempt
    pub initial_delay: Duration,
    /// Maximum delay between attempts
    pub max_delay: Duration,
    /// Multiplier for exponential backoff
    pub backoff_factor: f64,
    /// Maximum number of attempts (None = unlimited)
    pub max_attempts: Option<u32>,
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_secs(5),
            max_delay: Duration::from_secs(60),
            backoff_factor: 2.0,
            max_attempts: None, // Unlimited
        }
    }
}

/// Calculates the next reconnection delay using exponential backoff.
pub fn calculate_backoff(attempt: u32, config: &ReconnectConfig) -> Duration {
    let delay_secs = config.initial_delay.as_secs_f64()
        * config.backoff_factor.powi(attempt.saturating_sub(1) as i32);

    Duration::from_secs_f64(delay_secs.min(config.max_delay.as_secs_f64()))
}

/// Reconnection state for a peer.
#[derive(Debug, Clone)]
pub struct ReconnectState {
    /// Number of reconnection attempts
    pub attempts: u32,
    /// When to attempt next reconnection (ms since epoch)
    pub next_attempt_at: Option<u64>,
    /// Current backoff delay
    pub current_delay: Duration,
}

impl ReconnectState {
    /// Create new reconnection state.
    pub fn new() -> Self {
        Self {
            attempts: 0,
            next_attempt_at: None,
            current_delay: Duration::from_secs(5),
        }
    }

    /// Schedule next reconnection attempt.
    pub fn schedule_reconnect(&mut self, now_ms: u64, config: &ReconnectConfig) {
        self.attempts += 1;
        self.current_delay = calculate_backoff(self.attempts, config);
        self.next_attempt_at = Some(now_ms + self.current_delay.as_millis() as u64);
    }

    /// Reset state after successful connection.
    pub fn reset(&mut self) {
        self.attempts = 0;
        self.next_attempt_at = None;
        self.current_delay = Duration::from_secs(5);
    }

    /// Check if it's time to reconnect.
    pub fn should_reconnect(&self, now_ms: u64) -> bool {
        self.next_attempt_at.map(|t| now_ms >= t).unwrap_or(false)
    }

    /// Check if max attempts exceeded.
    pub fn exceeded_max_attempts(&self, config: &ReconnectConfig) -> bool {
        config
            .max_attempts
            .map(|max| self.attempts >= max)
            .unwrap_or(false)
    }
}

impl Default for ReconnectState {
    fn default() -> Self {
        Self::new()
    }
}

/// An outgoing connection to a remote peer.
pub struct OutgoingConnection {
    /// Remote address we connected to
    pub address: String,
    /// Our peer ID (for handshake)
    our_peer_id: String,
    /// Remote peer ID (known after handshake)
    pub remote_peer_id: Option<String>,
    /// Connection state
    pub state: OutgoingState,
    /// Write half of the WebSocket
    write: Option<
        Arc<Mutex<futures::stream::SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>>,
    >,
    /// Handle to the read task
    read_task: Option<JoinHandle<()>>,
    /// Reconnection state
    pub reconnect: ReconnectState,
}

impl OutgoingConnection {
    /// Create a new outgoing connection (not yet connected).
    pub fn new(address: String, our_peer_id: String) -> Self {
        Self {
            address,
            our_peer_id,
            remote_peer_id: None,
            state: OutgoingState::Connecting,
            write: None,
            read_task: None,
            reconnect: ReconnectState::new(),
        }
    }

    /// Connect to the remote peer.
    ///
    /// Returns Ok(()) if connection and handshake succeed.
    /// The connection will start receiving messages via the event channel.
    pub async fn connect(&mut self, event_tx: mpsc::UnboundedSender<ConnectionEvent>) -> Result<()> {
        self.state = OutgoingState::Connecting;

        // Connect to WebSocket
        let (ws_stream, _) = connect_async(&self.address).await?;

        self.state = OutgoingState::Handshaking;

        let (write, read) = ws_stream.split();
        let write = Arc::new(Mutex::new(write));
        self.write = Some(write.clone());

        // Send our handshake immediately
        let handshake = HandshakeMessage::new(&self.our_peer_id, "client");
        {
            let mut w = write.lock().await;
            w.send(Message::Binary(handshake.to_binary().into()))
                .await?;
        }

        // Spawn read task
        let addr = self.address.clone();
        let read_task = tokio::spawn(async move {
            Self::read_loop(addr, read, event_tx).await;
        });
        self.read_task = Some(read_task);

        // Handshake completion is async - we'll transition to Connected when we receive their handshake
        self.reconnect.reset();
        Ok(())
    }

    /// Read loop that forwards messages to the event channel.
    async fn read_loop(
        address: String,
        mut read: futures::stream::SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
        event_tx: mpsc::UnboundedSender<ConnectionEvent>,
    ) {
        loop {
            match read.next().await {
                Some(Ok(msg)) => {
                    let data = match msg {
                        Message::Binary(data) => data.to_vec(),
                        Message::Text(text) => text.into_bytes(),
                        Message::Ping(_) | Message::Pong(_) => continue,
                        Message::Close(_) => {
                            debug!("Received close frame from {}", address);
                            break;
                        }
                        Message::Frame(_) => continue,
                    };

                    // Check message size
                    if data.len() > MAX_MESSAGE_SIZE {
                        warn!(
                            "Message from {} exceeds max size ({} > {}), dropping",
                            address,
                            data.len(),
                            MAX_MESSAGE_SIZE
                        );
                        continue;
                    }

                    // Check if this is a handshake message
                    if let Some(handshake) = HandshakeMessage::from_binary(&data) {
                        debug!(
                            "Received handshake from {} (peer_id: {}, role: {})",
                            address, handshake.peer_id, handshake.role
                        );
                        let _ = event_tx.send(ConnectionEvent::Handshake {
                            temp_id: address.clone(),
                            peer_id: handshake.peer_id,
                        });
                    } else {
                        // Regular sync message
                        let _ = event_tx.send(ConnectionEvent::Message(
                            crate::connection::IncomingMessage {
                                temp_id: address.clone(),
                                data,
                            },
                        ));
                    }
                }
                Some(Err(e)) => {
                    match e {
                        WsError::ConnectionClosed | WsError::AlreadyClosed => {
                            debug!("Connection {} closed", address);
                        }
                        _ => {
                            error!("WebSocket error on {}: {}", address, e);
                        }
                    }
                    break;
                }
                None => {
                    debug!("Connection {} stream ended", address);
                    break;
                }
            }
        }

        // Notify that connection is closed
        let _ = event_tx.send(ConnectionEvent::Closed {
            temp_id: address.clone(),
        });
    }

    /// Mark that we received the remote peer's handshake.
    pub fn on_handshake_complete(&mut self, peer_id: String) {
        self.remote_peer_id = Some(peer_id);
        self.state = OutgoingState::Connected;
    }

    /// Send data to the remote peer.
    pub async fn send(&self, data: &[u8]) -> Result<()> {
        if let Some(write) = &self.write {
            let mut w = write.lock().await;
            w.send(Message::Binary(data.to_vec().into()))
                .await
                .map_err(|e| anyhow::anyhow!("Failed to send: {}", e))
        } else {
            Err(anyhow::anyhow!("Not connected"))
        }
    }

    /// Close the connection.
    pub async fn close(&mut self) {
        self.state = OutgoingState::Closed;

        // Send close frame
        if let Some(write) = &self.write {
            if let Ok(mut w) = write.try_lock() {
                let _ = w.send(Message::Close(None)).await;
            }
        }
        self.write = None;

        // Abort read task
        if let Some(task) = self.read_task.take() {
            task.abort();
        }
    }

    /// Prepare for reconnection (after disconnect).
    pub fn prepare_reconnect(&mut self, now_ms: u64, config: &ReconnectConfig) {
        self.state = OutgoingState::Reconnecting;
        self.remote_peer_id = None;
        self.write = None;
        if let Some(task) = self.read_task.take() {
            task.abort();
        }
        self.reconnect.schedule_reconnect(now_ms, config);
        info!(
            "Scheduled reconnect to {} in {:?} (attempt {})",
            self.address, self.reconnect.current_delay, self.reconnect.attempts
        );
    }

    /// Check if we should attempt reconnection now.
    pub fn should_reconnect(&self, now_ms: u64) -> bool {
        self.state == OutgoingState::Reconnecting && self.reconnect.should_reconnect(now_ms)
    }
}

impl Drop for OutgoingConnection {
    fn drop(&mut self) {
        if let Some(task) = self.read_task.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ==================== Backoff calculation ====================

    #[test]
    fn test_calculate_backoff_first_attempt() {
        let config = ReconnectConfig::default();
        let delay = calculate_backoff(1, &config);
        assert_eq!(delay, Duration::from_secs(5));
    }

    #[test]
    fn test_calculate_backoff_exponential() {
        let config = ReconnectConfig::default();

        // 5s, 10s, 20s, 40s, 60s (capped)
        assert_eq!(calculate_backoff(1, &config), Duration::from_secs(5));
        assert_eq!(calculate_backoff(2, &config), Duration::from_secs(10));
        assert_eq!(calculate_backoff(3, &config), Duration::from_secs(20));
        assert_eq!(calculate_backoff(4, &config), Duration::from_secs(40));
        assert_eq!(calculate_backoff(5, &config), Duration::from_secs(60)); // Capped at max
        assert_eq!(calculate_backoff(10, &config), Duration::from_secs(60)); // Still capped
    }

    #[test]
    fn test_calculate_backoff_custom_config() {
        let config = ReconnectConfig {
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(10),
            backoff_factor: 3.0,
            max_attempts: None,
        };

        // 1s, 3s, 9s, 10s (capped)
        assert_eq!(calculate_backoff(1, &config), Duration::from_secs(1));
        assert_eq!(calculate_backoff(2, &config), Duration::from_secs(3));
        assert_eq!(calculate_backoff(3, &config), Duration::from_secs(9));
        assert_eq!(calculate_backoff(4, &config), Duration::from_secs(10));
    }

    // ==================== ReconnectState ====================

    #[test]
    fn test_reconnect_state_new() {
        let state = ReconnectState::new();
        assert_eq!(state.attempts, 0);
        assert!(state.next_attempt_at.is_none());
    }

    #[test]
    fn test_schedule_reconnect() {
        let mut state = ReconnectState::new();
        let config = ReconnectConfig::default();

        state.schedule_reconnect(1000, &config);

        assert_eq!(state.attempts, 1);
        assert_eq!(state.next_attempt_at, Some(6000)); // 1000 + 5000ms
        assert_eq!(state.current_delay, Duration::from_secs(5));
    }

    #[test]
    fn test_schedule_reconnect_increments() {
        let mut state = ReconnectState::new();
        let config = ReconnectConfig::default();

        state.schedule_reconnect(0, &config);
        assert_eq!(state.attempts, 1);
        assert_eq!(state.current_delay, Duration::from_secs(5));

        state.schedule_reconnect(5000, &config);
        assert_eq!(state.attempts, 2);
        assert_eq!(state.current_delay, Duration::from_secs(10));
    }

    #[test]
    fn test_reconnect_state_reset() {
        let mut state = ReconnectState::new();
        let config = ReconnectConfig::default();

        state.schedule_reconnect(0, &config);
        state.schedule_reconnect(5000, &config);
        assert_eq!(state.attempts, 2);

        state.reset();

        assert_eq!(state.attempts, 0);
        assert!(state.next_attempt_at.is_none());
    }

    #[test]
    fn test_should_reconnect() {
        let mut state = ReconnectState::new();
        let config = ReconnectConfig::default();

        // Not scheduled yet
        assert!(!state.should_reconnect(10000));

        state.schedule_reconnect(1000, &config);

        // Too early
        assert!(!state.should_reconnect(3000));

        // Ready
        assert!(state.should_reconnect(6000));
        assert!(state.should_reconnect(10000));
    }

    #[test]
    fn test_exceeded_max_attempts() {
        let state = ReconnectState {
            attempts: 5,
            next_attempt_at: None,
            current_delay: Duration::from_secs(60),
        };

        let unlimited = ReconnectConfig::default();
        assert!(!state.exceeded_max_attempts(&unlimited));

        let limited = ReconnectConfig {
            max_attempts: Some(5),
            ..Default::default()
        };
        assert!(state.exceeded_max_attempts(&limited));

        let more = ReconnectConfig {
            max_attempts: Some(10),
            ..Default::default()
        };
        assert!(!state.exceeded_max_attempts(&more));
    }

    // ==================== OutgoingConnection state ====================

    #[test]
    fn test_outgoing_connection_new() {
        let conn = OutgoingConnection::new("ws://localhost:8080".into(), "our-peer".into());

        assert_eq!(conn.address, "ws://localhost:8080");
        assert_eq!(conn.state, OutgoingState::Connecting);
        assert!(conn.remote_peer_id.is_none());
    }

    #[test]
    fn test_on_handshake_complete() {
        let mut conn = OutgoingConnection::new("ws://localhost:8080".into(), "our-peer".into());
        conn.state = OutgoingState::Handshaking;

        conn.on_handshake_complete("remote-peer".into());

        assert_eq!(conn.state, OutgoingState::Connected);
        assert_eq!(conn.remote_peer_id, Some("remote-peer".into()));
    }

    #[test]
    fn test_prepare_reconnect() {
        let mut conn = OutgoingConnection::new("ws://localhost:8080".into(), "our-peer".into());
        let config = ReconnectConfig::default();

        conn.prepare_reconnect(1000, &config);

        assert_eq!(conn.state, OutgoingState::Reconnecting);
        assert_eq!(conn.reconnect.attempts, 1);
        assert!(conn.should_reconnect(6000));
    }
}
