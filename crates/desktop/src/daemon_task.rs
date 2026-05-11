//! Cancellation-token-aware daemon spawn helper.
//!
//! Spawns `sync_daemon::daemon::run_with_shutdown` as a tokio task inside
//! Tauri's runtime, and sets up a watchdog task that exits the application if
//! the daemon task fails or panics. This surfaces daemon startup errors (e.g.
//! "another instance already holds the lock") to the user as a visible app exit
//! rather than a silent zombie tray icon.

use anyhow::Result;
use std::sync::Arc;
use sync_daemon::daemon::DaemonRunConfig;
use tauri::AppHandle;
use tauri::async_runtime::JoinHandle;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// Handle to the running daemon task, accessible to the Quit handler.
pub struct DaemonHandle {
    pub token: CancellationToken,
    join: Arc<Mutex<Option<JoinHandle<Result<()>>>>>,
}

impl DaemonHandle {
    /// Spawn the daemon task and start the watchdog.
    ///
    /// If the daemon task exits with an error or panics, the watchdog calls
    /// `app.exit(1)` so the tray icon disappears instead of advertising a
    /// broken daemon.
    pub fn spawn(config: DaemonRunConfig, app: AppHandle) -> Self {
        let token = CancellationToken::new();
        // Use Tauri's async runtime so the spawn works from the setup() callback,
        // which runs before the Tokio reactor is entered on the main thread.
        let join_handle = tauri::async_runtime::spawn(sync_daemon::daemon::run_with_shutdown(
            config,
            token.clone(),
        ));

        let join = Arc::new(Mutex::new(Some(join_handle)));
        let watchdog_join = join.clone();
        let watchdog_app = app.clone();

        tauri::async_runtime::spawn(async move {
            let handle = {
                let mut guard = watchdog_join.lock().await;
                guard.take()
            };

            if let Some(h) = handle {
                match h.await {
                    Ok(Ok(())) => {
                        // Daemon exited cleanly — this happens on normal Quit.
                        info!("Daemon task exited cleanly");
                    }
                    Ok(Err(e)) => {
                        // Daemon returned an error (e.g. failed to acquire lock).
                        error!("Daemon exited with error: {e:#}");
                        watchdog_app.exit(1);
                    }
                    Err(e) => {
                        // Task panicked.
                        error!("Daemon task panicked: {e}");
                        watchdog_app.exit(1);
                    }
                }
            }
        });

        DaemonHandle { token, join }
    }

    /// Request daemon shutdown and wait up to `timeout` for the task to finish.
    ///
    /// On timeout, logs a warning and returns; the caller should then call
    /// `app.exit()` regardless so the process exits even if the daemon hung.
    pub async fn shutdown(&self, timeout: std::time::Duration) {
        self.token.cancel();

        let handle = {
            let mut guard = self.join.lock().await;
            guard.take()
        };

        if let Some(h) = handle {
            match tokio::time::timeout(timeout, h).await {
                Ok(Ok(Ok(()))) => {
                    info!("Daemon shut down cleanly");
                }
                Ok(Ok(Err(e))) => {
                    warn!("Daemon returned error during shutdown: {e:#}");
                }
                Ok(Err(e)) => {
                    warn!("Daemon task panicked during shutdown: {e}");
                }
                Err(_) => {
                    warn!(
                        "Daemon shutdown exceeded {}s — force-exiting",
                        timeout.as_secs()
                    );
                }
            }
        }
    }
}
