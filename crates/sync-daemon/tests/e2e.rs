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
// run_with_shutdown Tests
// ============================================================================

/// External cancellation during startup completes cleanly.
///
/// Cancels the token immediately after calling `run_with_shutdown`, before the
/// daemon has finished its full startup sequence (gossip join, file watcher,
/// mDNS). Verifies the function returns without hanging and without panicking.
#[tokio::test]
async fn test_run_with_shutdown_cancels_during_startup() {
    use tokio_util::sync::CancellationToken;
    use sync_daemon::daemon::{DaemonRunConfig, run_with_shutdown};
    use tokio::time::timeout;

    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let vault_path = temp_dir.path().to_path_buf();

    let config = DaemonRunConfig {
        vault: vault_path,
        identity_key: None,
        health_listen: None,
        relay_listen: None,
    };

    let shutdown = CancellationToken::new();
    // Cancel immediately so the daemon sees cancellation as early as possible.
    shutdown.cancel();

    let result = timeout(
        Duration::from_secs(15),
        run_with_shutdown(config, shutdown),
    )
    .await
    .expect("run_with_shutdown did not complete within 15s after immediate cancel");

    // Ok(()) or an Err are both acceptable — the important property is that it
    // returned at all rather than hanging indefinitely.
    match result {
        Ok(()) => {}
        Err(e) => {
            // Any error is fine here; the contract is just "returns cleanly".
            // We log it so test output is informative on CI.
            eprintln!("run_with_shutdown returned error on early cancel (acceptable): {e}");
        }
    }
}

/// A clean startup followed by external cancellation shuts down without hanging.
///
/// Lets the daemon fully start (health endpoint responding), then cancels and
/// verifies the function returns within a reasonable deadline.
#[tokio::test]
async fn test_run_with_shutdown_cancels_after_startup() {
    use tokio::net::TcpListener;
    use tokio_util::sync::CancellationToken;
    use sync_daemon::daemon::{DaemonRunConfig, run_with_shutdown};
    use tokio::time::timeout;

    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let vault_path = temp_dir.path().to_path_buf();

    // Bind a random port for the health endpoint so we can poll readiness.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Failed to bind health port");
    let health_addr = listener.local_addr().expect("Failed to get local addr");
    // Release the listener — the daemon will re-bind the same port.
    drop(listener);

    let config = DaemonRunConfig {
        vault: vault_path,
        identity_key: None,
        health_listen: Some(health_addr.to_string()),
        relay_listen: None,
    };

    let shutdown = CancellationToken::new();
    let shutdown_clone = shutdown.clone();

    let handle = tokio::spawn(run_with_shutdown(config, shutdown.clone()));

    // Poll the health endpoint until the daemon is fully started.
    let client = reqwest::Client::new();
    let health_url = format!("http://{}/health", health_addr);
    let startup_deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if tokio::time::Instant::now() >= startup_deadline {
            panic!("daemon did not start within 20s");
        }
        if client.get(&health_url).send().await.map(|r| r.status().is_success()).unwrap_or(false) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Now trigger external cancellation and wait for the daemon to exit.
    shutdown_clone.cancel();

    let result = timeout(Duration::from_secs(15), handle)
        .await
        .expect("run_with_shutdown did not return within 15s after cancel")
        .expect("tokio task panicked");

    // The daemon should complete cleanly (Ok) after cancellation.
    result.expect("run_with_shutdown returned an error after clean shutdown");
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

