//! End-to-end tests for sync-daemon.
//!
//! Tests the full daemon behavior: WebSocket connections, handshakes,
//! file watching, and sync message handling.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use sync_daemon::{
    message::HandshakeMessage, native_fs::NativeFs, server::ServerEvent,
    server::WebSocketServer, watcher::FileWatcher, FileEventKind,
};
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

/// Test client that connects to the daemon.
struct TestClient {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    peer_id: String,
}

impl TestClient {
    /// Connect to the daemon and complete handshake.
    async fn connect_and_handshake(addr: SocketAddr) -> Self {
        let url = format!("ws://{}", addr);
        let (ws, _) = connect_async(&url).await.expect("Failed to connect");

        let mut client = Self {
            ws,
            peer_id: format!("test-client-{}", uuid::Uuid::new_v4()),
        };

        // Receive server handshake
        let server_hs = client.expect_handshake().await;
        assert_eq!(server_hs.role, "server", "Server should send server role");

        // Send our handshake
        let our_hs = HandshakeMessage::new(&client.peer_id, "client");
        client.send_binary(&our_hs.to_binary()).await;

        client
    }

    /// Connect and handshake with a specific address in our handshake.
    async fn connect_with_address(addr: SocketAddr, address: &str) -> Self {
        let url = format!("ws://{}", addr);
        let (ws, _) = connect_async(&url).await.expect("Failed to connect");

        let mut client = Self {
            ws,
            peer_id: format!("test-client-{}", uuid::Uuid::new_v4()),
        };

        // Receive server handshake
        let server_hs = client.expect_handshake().await;
        assert_eq!(server_hs.role, "server");

        // Send handshake with address
        let our_hs = HandshakeMessage::with_address(&client.peer_id, "client", address);
        client.send_binary(&our_hs.to_binary()).await;

        client
    }

    /// Receive and parse handshake message.
    async fn expect_handshake(&mut self) -> HandshakeMessage {
        let msg = self.recv_message().await;
        HandshakeMessage::from_binary(&msg).expect("Expected handshake message")
    }

    /// Receive binary message.
    async fn recv_message(&mut self) -> Vec<u8> {
        loop {
            match self.ws.next().await {
                Some(Ok(Message::Binary(data))) => return data.to_vec(),
                Some(Ok(Message::Text(text))) => return text.into_bytes(),
                Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => continue,
                Some(Ok(Message::Close(_))) => panic!("Connection closed unexpectedly"),
                Some(Err(e)) => panic!("WebSocket error: {}", e),
                None => panic!("Stream ended unexpectedly"),
                _ => continue,
            }
        }
    }

    /// Receive message with timeout.
    async fn recv_message_timeout(&mut self, duration: Duration) -> Result<Vec<u8>, &'static str> {
        match timeout(duration, self.recv_message()).await {
            Ok(msg) => Ok(msg),
            Err(_) => Err("Timeout waiting for message"),
        }
    }

    /// Send binary message.
    async fn send_binary(&mut self, data: &[u8]) {
        self.ws
            .send(Message::Binary(data.to_vec().into()))
            .await
            .expect("Failed to send message");
    }

    /// Close connection gracefully.
    async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// Create a server and listener bound to a random port.
async fn create_server(peer_id: &str) -> (WebSocketServer, tokio::net::TcpListener, SocketAddr) {
    let server = WebSocketServer::new(peer_id.to_string(), None);
    let listener = WebSocketServer::bind("127.0.0.1:0")
        .await
        .expect("Failed to bind");
    let addr = listener.local_addr().expect("Failed to get local addr");
    (server, listener, addr)
}

/// Poll for a ServerEvent with a timeout.
async fn poll_event_timeout(
    server: &mut WebSocketServer,
    duration: Duration,
) -> Option<ServerEvent> {
    timeout(duration, server.poll_event()).await.ok().flatten()
}

// ============================================================================
// ServerEvent Tests (poll_event API)
// ============================================================================

#[tokio::test]
async fn test_poll_event_peer_connected() {
    let (server, listener, addr) = create_server("test-server").await;

    // Accept connection in background
    let listener = Arc::new(listener);
    let listener_clone = Arc::clone(&listener);
    let server = Arc::new(Mutex::new(server));
    let server_clone = Arc::clone(&server);

    let accept_handle = tokio::spawn(async move {
        let (stream, peer_addr) = listener_clone.accept().await.expect("Failed to accept");
        server_clone
            .lock()
            .await
            .accept_connection(stream, peer_addr)
            .await;
    });

    // Connect client with address
    let client =
        TestClient::connect_with_address(addr, "ws://192.168.1.10:9427").await;

    accept_handle.await.expect("Accept task failed");

    // poll_event should return PeerConnected
    let mut guard = server.lock().await;
    let event = poll_event_timeout(&mut guard, Duration::from_secs(2))
        .await
        .expect("Should receive PeerConnected");

    match event {
        ServerEvent::PeerConnected { peer_id, address } => {
            assert_eq!(peer_id, client.peer_id);
            assert_eq!(address.as_deref(), Some("ws://192.168.1.10:9427"));
        }
        other => panic!("Expected PeerConnected, got {:?}", other),
    }

    assert_eq!(guard.peer_count(), 1);

    client.close().await;
}

#[tokio::test]
async fn test_poll_event_message() {
    let (server, listener, addr) = create_server("test-server").await;

    let listener = Arc::new(listener);
    let listener_clone = Arc::clone(&listener);
    let server = Arc::new(Mutex::new(server));
    let server_clone = Arc::clone(&server);

    let accept_handle = tokio::spawn(async move {
        let (stream, peer_addr) = listener_clone.accept().await.expect("Failed to accept");
        server_clone
            .lock()
            .await
            .accept_connection(stream, peer_addr)
            .await;
    });

    let mut client = TestClient::connect_and_handshake(addr).await;
    accept_handle.await.expect("Accept task failed");

    // Drain the PeerConnected event
    let mut guard = server.lock().await;
    let event = poll_event_timeout(&mut guard, Duration::from_secs(2))
        .await
        .expect("Should receive PeerConnected");
    assert!(matches!(event, ServerEvent::PeerConnected { .. }));

    let client_peer_id = client.peer_id.clone();
    drop(guard);

    // Send a message from client
    client.send_binary(b"hello from client").await;

    // poll_event should return Message with resolved peer_id
    let mut guard = server.lock().await;
    let event = poll_event_timeout(&mut guard, Duration::from_secs(2))
        .await
        .expect("Should receive Message");

    match event {
        ServerEvent::Message(msg) => {
            assert_eq!(msg.peer_id, client_peer_id, "Message should have resolved peer_id");
            assert_eq!(msg.data, b"hello from client");
        }
        other => panic!("Expected Message, got {:?}", other),
    }

    drop(guard);
    client.close().await;
}

#[tokio::test]
async fn test_poll_event_peer_disconnected() {
    let (server, listener, addr) = create_server("test-server").await;

    let listener = Arc::new(listener);
    let listener_clone = Arc::clone(&listener);
    let server = Arc::new(Mutex::new(server));
    let server_clone = Arc::clone(&server);

    let accept_handle = tokio::spawn(async move {
        let (stream, peer_addr) = listener_clone.accept().await.expect("Failed to accept");
        server_clone
            .lock()
            .await
            .accept_connection(stream, peer_addr)
            .await;
    });

    let client = TestClient::connect_and_handshake(addr).await;
    accept_handle.await.expect("Accept task failed");

    // Drain PeerConnected
    let mut guard = server.lock().await;
    let event = poll_event_timeout(&mut guard, Duration::from_secs(2))
        .await
        .expect("Should receive PeerConnected");
    let connected_peer_id = match event {
        ServerEvent::PeerConnected { peer_id, .. } => peer_id,
        other => panic!("Expected PeerConnected, got {:?}", other),
    };
    drop(guard);

    // Client disconnects
    client.close().await;

    // poll_event should return PeerDisconnected
    let mut guard = server.lock().await;
    let event = poll_event_timeout(&mut guard, Duration::from_secs(2))
        .await
        .expect("Should receive PeerDisconnected");

    match event {
        ServerEvent::PeerDisconnected { peer_id } => {
            assert_eq!(peer_id, connected_peer_id);
        }
        other => panic!("Expected PeerDisconnected, got {:?}", other),
    }

    assert_eq!(guard.peer_count(), 0);
}

#[tokio::test]
async fn test_pre_handshake_close_not_emitted() {
    let (server, listener, addr) = create_server("test-server").await;

    let listener = Arc::new(listener);
    let listener_clone = Arc::clone(&listener);
    let server = Arc::new(Mutex::new(server));
    let server_clone = Arc::clone(&server);

    let accept_handle = tokio::spawn(async move {
        let (stream, peer_addr) = listener_clone.accept().await.expect("Failed to accept");
        server_clone
            .lock()
            .await
            .accept_connection(stream, peer_addr)
            .await;
    });

    // Connect but DON'T send handshake, just close immediately
    let url = format!("ws://{}", addr);
    let (mut ws, _) = connect_async(&url).await.expect("Failed to connect");

    // Receive the server handshake (server sends first)
    let _ = ws.next().await;

    // Close without sending our handshake
    let _ = ws.close(None).await;

    accept_handle.await.expect("Accept task failed");

    // poll_event should NOT return PeerDisconnected (pre-handshake close is silent)
    // Instead it should time out
    let mut guard = server.lock().await;
    let result = timeout(Duration::from_millis(500), guard.poll_event()).await;
    assert!(result.is_err(), "Should not emit event for pre-handshake close");
}

#[tokio::test]
async fn test_broadcast_except_by_peer_id() {
    let (server, listener, addr) = create_server("test-server").await;

    let listener = Arc::new(listener);
    let server = Arc::new(Mutex::new(server));

    // Accept two connections
    let listener_clone = Arc::clone(&listener);
    let server_clone = Arc::clone(&server);
    let accept_handle = tokio::spawn(async move {
        for _ in 0..2 {
            let (stream, peer_addr) = listener_clone.accept().await.expect("Failed to accept");
            server_clone
                .lock()
                .await
                .accept_connection(stream, peer_addr)
                .await;
        }
    });

    let mut client1 = TestClient::connect_and_handshake(addr).await;
    let mut client2 = TestClient::connect_and_handshake(addr).await;

    accept_handle.await.expect("Accept task failed");

    // Drain both PeerConnected events
    let mut guard = server.lock().await;
    let event1 = poll_event_timeout(&mut guard, Duration::from_secs(2))
        .await
        .expect("Should receive PeerConnected 1");
    let peer_id_1 = match event1 {
        ServerEvent::PeerConnected { peer_id, .. } => peer_id,
        other => panic!("Expected PeerConnected, got {:?}", other),
    };
    let event2 = poll_event_timeout(&mut guard, Duration::from_secs(2))
        .await
        .expect("Should receive PeerConnected 2");
    assert!(matches!(event2, ServerEvent::PeerConnected { .. }));

    // broadcast_except(peer_id_1) should only reach client 2
    guard
        .broadcast_except(b"only for client 2", &peer_id_1)
        .await;
    drop(guard);

    // Client 2 should receive the message
    let msg2 = client2
        .recv_message_timeout(Duration::from_secs(2))
        .await
        .expect("Client 2 should receive broadcast");
    assert_eq!(msg2, b"only for client 2");

    // Client 1 should NOT receive it
    let msg1 = client1
        .recv_message_timeout(Duration::from_millis(300))
        .await;
    assert!(msg1.is_err(), "Client 1 should not receive the broadcast");

    client1.close().await;
    client2.close().await;
}

// ============================================================================
// Migrated Tests (from recv_event to poll_event)
// ============================================================================

#[tokio::test]
async fn test_handshake_exchange() {
    let (server, listener, addr) = create_server("test-server").await;

    let listener = Arc::new(listener);
    let server = Arc::new(Mutex::new(server));
    let server_clone = Arc::clone(&server);

    let listener_clone = Arc::clone(&listener);
    let accept_handle = tokio::spawn(async move {
        let (stream, peer_addr) = listener_clone.accept().await.expect("Failed to accept");
        server_clone
            .lock()
            .await
            .accept_connection(stream, peer_addr)
            .await;
    });

    let client = TestClient::connect_and_handshake(addr).await;
    accept_handle.await.expect("Accept task failed");

    // poll_event should produce PeerConnected
    let mut guard = server.lock().await;
    let event = poll_event_timeout(&mut guard, Duration::from_secs(2))
        .await
        .expect("Should receive PeerConnected");
    assert!(matches!(event, ServerEvent::PeerConnected { .. }));
    assert_eq!(guard.peer_count(), 1, "Should have one peer after handshake");

    drop(guard);
    client.close().await;
}

#[tokio::test]
async fn test_multiple_clients() {
    let (server, listener, addr) = create_server("test-server").await;

    let listener = Arc::new(listener);
    let server = Arc::new(Mutex::new(server));

    let listener_clone = Arc::clone(&listener);
    let server_clone = Arc::clone(&server);
    let accept_handle = tokio::spawn(async move {
        for _ in 0..2 {
            let (stream, peer_addr) = listener_clone.accept().await.expect("Failed to accept");
            server_clone
                .lock()
                .await
                .accept_connection(stream, peer_addr)
                .await;
        }
    });

    let client1 = TestClient::connect_and_handshake(addr).await;
    let client2 = TestClient::connect_and_handshake(addr).await;

    accept_handle.await.expect("Accept task failed");

    // Drain both PeerConnected events
    let mut guard = server.lock().await;
    for _ in 0..2 {
        let event = poll_event_timeout(&mut guard, Duration::from_secs(2))
            .await
            .expect("Should receive PeerConnected");
        assert!(matches!(event, ServerEvent::PeerConnected { .. }));
    }
    assert_eq!(guard.peer_count(), 2, "Should have two peers");

    drop(guard);
    client1.close().await;
    client2.close().await;
}

#[tokio::test]
async fn test_message_broadcast() {
    let (server, listener, addr) = create_server("test-server").await;

    let listener = Arc::new(listener);
    let server = Arc::new(Mutex::new(server));

    let listener_clone = Arc::clone(&listener);
    let server_clone = Arc::clone(&server);
    let accept_handle = tokio::spawn(async move {
        for _ in 0..2 {
            let (stream, peer_addr) = listener_clone.accept().await.expect("Failed to accept");
            server_clone
                .lock()
                .await
                .accept_connection(stream, peer_addr)
                .await;
        }
    });

    let mut client1 = TestClient::connect_and_handshake(addr).await;
    let mut client2 = TestClient::connect_and_handshake(addr).await;

    accept_handle.await.expect("Accept task failed");

    // Drain PeerConnected events
    let mut guard = server.lock().await;
    for _ in 0..2 {
        let event = poll_event_timeout(&mut guard, Duration::from_secs(2))
            .await
            .expect("Should receive PeerConnected");
        assert!(matches!(event, ServerEvent::PeerConnected { .. }));
    }

    // Broadcast from server
    let test_message = b"broadcast test";
    guard.broadcast(test_message).await;
    drop(guard);

    // Both clients should receive it
    let msg1 = client1
        .recv_message_timeout(Duration::from_secs(2))
        .await
        .expect("Client 1 should receive broadcast");
    let msg2 = client2
        .recv_message_timeout(Duration::from_secs(2))
        .await
        .expect("Client 2 should receive broadcast");

    assert_eq!(msg1, test_message);
    assert_eq!(msg2, test_message);

    client1.close().await;
    client2.close().await;
}

#[tokio::test]
async fn test_connection_events_flow() {
    let (server, listener, addr) = create_server("test-server").await;

    let listener = Arc::new(listener);
    let server = Arc::new(Mutex::new(server));
    let server_clone = Arc::clone(&server);

    let listener_clone = Arc::clone(&listener);
    let accept_handle = tokio::spawn(async move {
        let (stream, peer_addr) = listener_clone.accept().await.expect("Failed to accept");
        server_clone
            .lock()
            .await
            .accept_connection(stream, peer_addr)
            .await;
    });

    let client = TestClient::connect_and_handshake(addr).await;
    accept_handle.await.expect("Accept task failed");

    // Should receive PeerConnected via poll_event
    let mut guard = server.lock().await;
    let event = poll_event_timeout(&mut guard, Duration::from_secs(2))
        .await
        .expect("Should receive PeerConnected");

    match event {
        ServerEvent::PeerConnected { peer_id, .. } => {
            assert!(!peer_id.is_empty(), "Peer ID should not be empty");
        }
        other => panic!("Expected PeerConnected, got {:?}", other),
    }

    drop(guard);
    client.close().await;
}

// ============================================================================
// File Watcher Tests (unchanged)
// ============================================================================

/// Test file watcher detects changes.
#[tokio::test]
async fn test_file_watcher_detects_changes() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let vault_path = temp_dir.path().to_path_buf();

    // Create watcher first, let it initialize
    let mut watcher = FileWatcher::new(vault_path.clone()).expect("Failed to create watcher");

    // Give watcher time to fully initialize - FSEvents on macOS needs time
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Write a file using sync fs to ensure atomic write
    let test_file = vault_path.join("test.md");
    std::fs::write(&test_file, "# Hello").expect("Failed to write file");

    // Force a second modification to trigger FSEvents reliably
    tokio::time::sleep(Duration::from_millis(100)).await;
    std::fs::write(&test_file, "# Hello World").expect("Failed to modify file");

    // Wait for event - FSEvents + debounce can take several seconds
    let event = timeout(Duration::from_secs(10), watcher.event_rx().recv())
        .await
        .expect("Timeout waiting for file event")
        .expect("No event received");

    assert_eq!(event.path, "test.md");
    assert_eq!(event.kind, FileEventKind::Modified);
}

/// Test that file watcher ignores .sync directory.
#[tokio::test]
async fn test_file_watcher_ignores_sync_directory() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let vault_path = temp_dir.path().to_path_buf();

    // Create .sync directory before watcher starts
    let sync_dir = vault_path.join(".sync");
    std::fs::create_dir_all(&sync_dir).expect("Failed to create .sync dir");

    // Create watcher
    let mut watcher = FileWatcher::new(vault_path.clone()).expect("Failed to create watcher");

    // Give watcher time to fully initialize
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Write to .sync directory (should be ignored)
    let sync_file = sync_dir.join("state.json");
    std::fs::write(&sync_file, "{}").expect("Failed to write sync file");

    // Wait a bit, then write to vault root (should be detected)
    tokio::time::sleep(Duration::from_millis(200)).await;
    let test_file = vault_path.join("test.md");
    std::fs::write(&test_file, "# Hello").expect("Failed to write file");

    // Modify again to ensure FSEvents triggers
    tokio::time::sleep(Duration::from_millis(100)).await;
    std::fs::write(&test_file, "# Hello World").expect("Failed to modify file");

    // Should only get the test.md event
    let event = timeout(Duration::from_secs(10), watcher.event_rx().recv())
        .await
        .expect("Timeout waiting for file event")
        .expect("No event received");

    assert_eq!(event.path, "test.md", "Should detect test.md, not .sync file");
}

/// Test that file watcher only processes .md files.
#[tokio::test]
async fn test_file_watcher_only_md_files() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let vault_path = temp_dir.path().to_path_buf();

    let mut watcher = FileWatcher::new(vault_path.clone()).expect("Failed to create watcher");

    // Give watcher time to fully initialize
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Write non-md file (should be ignored)
    let txt_file = vault_path.join("test.txt");
    std::fs::write(&txt_file, "text").expect("Failed to write txt file");

    // Wait a bit, then write md file (should be detected)
    tokio::time::sleep(Duration::from_millis(200)).await;
    let md_file = vault_path.join("test.md");
    std::fs::write(&md_file, "# Markdown").expect("Failed to write md file");

    // Modify again to ensure FSEvents triggers
    tokio::time::sleep(Duration::from_millis(100)).await;
    std::fs::write(&md_file, "# Markdown Updated").expect("Failed to modify md file");

    // Should only get the .md event
    let event = timeout(Duration::from_secs(10), watcher.event_rx().recv())
        .await
        .expect("Timeout waiting for file event")
        .expect("No event received");

    assert_eq!(event.path, "test.md");
}

// ============================================================================
// Other Tests (unchanged)
// ============================================================================

#[tokio::test]
async fn test_handshake_message_roundtrip() {
    let original = HandshakeMessage::new("peer-123", "server");
    let binary = original.to_binary();
    let parsed = HandshakeMessage::from_binary(&binary).expect("Should parse valid handshake");

    assert_eq!(parsed.peer_id, "peer-123");
    assert_eq!(parsed.role, "server");
    assert_eq!(parsed.msg_type, "handshake");
}

#[tokio::test]
async fn test_handshake_rejects_invalid_json() {
    let invalid = b"not json at all";
    assert!(
        HandshakeMessage::from_binary(invalid).is_none(),
        "Should reject invalid JSON"
    );
}

#[tokio::test]
async fn test_handshake_rejects_non_handshake_message() {
    let other_json = b"{\"type\": \"other\", \"data\": 123}";
    assert!(
        HandshakeMessage::from_binary(other_json).is_none(),
        "Should reject non-handshake JSON"
    );
}

#[tokio::test]
async fn test_native_fs_basic_operations() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let fs = NativeFs::new(temp_dir.path().to_path_buf());

    use sync_core::fs::FileSystem;

    // Write
    fs.write("test.md", b"# Hello").await.expect("Write failed");

    // Exists
    assert!(fs.exists("test.md").await.expect("Exists check failed"));
    assert!(!fs.exists("nonexistent.md").await.expect("Exists check failed"));

    // Read
    let content = fs.read("test.md").await.expect("Read failed");
    assert_eq!(content, b"# Hello");

    // List
    let files = fs.list(".").await.expect("List failed");
    assert!(files.iter().any(|f| f.name == "test.md"));

    // Delete
    fs.delete("test.md").await.expect("Delete failed");
    assert!(!fs.exists("test.md").await.expect("Exists check failed"));
}

#[tokio::test]
async fn test_native_fs_nested_directories() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let fs = NativeFs::new(temp_dir.path().to_path_buf());

    use sync_core::fs::FileSystem;

    // Write to nested path (should create directories)
    fs.write("knowledge/topic.md", b"# Topic")
        .await
        .expect("Write to nested path failed");

    assert!(fs.exists("knowledge/topic.md").await.expect("Exists check failed"));

    let content = fs.read("knowledge/topic.md").await.expect("Read failed");
    assert_eq!(content, b"# Topic");
}

// ============================================================================
// Message Size Limit Tests
// ============================================================================

#[tokio::test]
async fn test_max_message_size_constant() {
    use sync_daemon::MAX_MESSAGE_SIZE;

    // 50 MB limit
    assert_eq!(MAX_MESSAGE_SIZE, 50 * 1024 * 1024);
}
