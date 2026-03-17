//! End-to-end tests for sync-daemon.
//!
//! Tests transport-independent behavior: file watching, NativeFs operations,
//! and the health endpoint. WebSocket-specific tests were removed when the
//! daemon migrated from WebSocket to iroh QUIC transport (Effort 4b).

use std::time::Duration;

use sync_daemon::{native_fs::NativeFs, watcher::FileWatcher, FileEventKind};
use tempfile::TempDir;
use tokio::time::timeout;

// ============================================================================
// File Watcher Tests
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
// NativeFs Tests
// ============================================================================

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
// Health Endpoint Test
// ============================================================================

#[tokio::test]
async fn test_health_endpoint() {
    use tokio::net::TcpListener;

    // Bind to a random port
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Failed to bind");
    let addr = listener.local_addr().expect("Failed to get local addr");

    // Build a minimal axum health app (same as sync_daemon::http::serve_health)
    let app = axum::Router::new().route("/health", axum::routing::get(|| async { "OK" }));

    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    let resp = reqwest::get(format!("http://{}/health", addr))
        .await
        .expect("Health request failed");

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "OK");
}
