//! Cancellation-token-aware daemon spawn helper.
//!
//! Spawns `sync_daemon::daemon::run_with_shutdown` as a tokio task inside
//! Tauri's runtime, and sets up a watchdog task that exits the application if
//! the daemon task fails or panics. This surfaces daemon startup errors (e.g.
//! "another instance already holds the lock") to the user as a visible app exit
//! rather than a silent zombie tray icon.

use sync_daemon::daemon::DaemonRunConfig;
use tauri::AppHandle;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// Handle to the running daemon task, accessible to the Quit handler.
pub struct DaemonHandle {
    pub token: CancellationToken,
    /// Receives a signal when the watchdog has observed the daemon task exit.
    /// The watchdog holds the JoinHandle exclusively; shutdown() waits on this.
    done_rx: oneshot::Receiver<()>,
}

impl DaemonHandle {
    /// Spawn the daemon task and start the watchdog.
    ///
    /// The watchdog holds sole ownership of the JoinHandle so there is no
    /// double-take race between the watchdog and `shutdown()`. When the daemon
    /// task exits (for any reason), the watchdog fires the `done_tx` oneshot
    /// and, if the exit was an error, also calls `app.exit(1)`.
    pub fn spawn(config: DaemonRunConfig, app: AppHandle) -> Self {
        let token = CancellationToken::new();
        // Use Tauri's async runtime so the spawn works from the setup() callback,
        // which runs before the Tokio reactor is entered on the main thread.
        let join_handle = tauri::async_runtime::spawn(sync_daemon::daemon::run_with_shutdown(
            config,
            token.clone(),
        ));

        let (done_tx, done_rx) = oneshot::channel::<()>();

        tauri::async_runtime::spawn(async move {
            match join_handle.await {
                Ok(Ok(())) => {
                    // Daemon exited cleanly — this happens on normal Quit.
                    info!("Daemon task exited cleanly");
                }
                Ok(Err(e)) => {
                    // Daemon returned an error (e.g. failed to acquire lock).
                    error!("Daemon exited with error: {e:#}");
                    app.exit(1);
                }
                Err(e) => {
                    // Task panicked.
                    error!("Daemon task panicked: {e}");
                    app.exit(1);
                }
            }
            // Signal shutdown() that the task has fully exited. The send may
            // fail if DaemonHandle was dropped (app already exiting), which is fine.
            let _ = done_tx.send(());
        });

        DaemonHandle { token, done_rx }
    }

    /// Construct a handle from raw components — used in tests only.
    #[cfg(test)]
    pub fn from_parts(token: CancellationToken, done_rx: oneshot::Receiver<()>) -> Self {
        DaemonHandle { token, done_rx }
    }

    /// Request daemon shutdown and wait up to `timeout` for the task to finish.
    ///
    /// On timeout, logs a warning and returns; the caller should then call
    /// `app.exit()` regardless so the process exits even if the daemon hung.
    pub async fn shutdown(self, timeout: std::time::Duration) {
        self.token.cancel();

        match tokio::time::timeout(timeout, self.done_rx).await {
            Ok(Ok(())) => {
                info!("Daemon shut down cleanly");
            }
            Ok(Err(_)) => {
                // Sender dropped — watchdog exited without firing (shouldn't happen
                // in normal flow, but safe to treat as "daemon is done").
                info!("Daemon task completed (watchdog exited)");
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// shutdown() waits for done_rx and returns cleanly when the watchdog fires.
    #[tokio::test]
    async fn test_shutdown_waits_for_watchdog() {
        let token = CancellationToken::new();
        let (done_tx, done_rx) = oneshot::channel::<()>();
        let handle = DaemonHandle::from_parts(token.clone(), done_rx);

        // Fire the "watchdog done" signal after a short delay.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = done_tx.send(());
        });

        // shutdown() should block until done_rx fires, then return before the timeout.
        let start = std::time::Instant::now();
        handle.shutdown(Duration::from_secs(5)).await;

        // Should have taken ~20ms, well under the 5s timeout.
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(token.is_cancelled());
    }

    /// shutdown() returns after the timeout if the watchdog never fires.
    #[tokio::test]
    async fn test_shutdown_times_out_if_daemon_hangs() {
        let token = CancellationToken::new();
        let (_done_tx, done_rx) = oneshot::channel::<()>(); // never sent
        let handle = DaemonHandle::from_parts(token, done_rx);

        // Use a short timeout so the test doesn't take long.
        let start = std::time::Instant::now();
        handle.shutdown(Duration::from_millis(100)).await;

        // Should have taken ~100ms (the timeout), not hung indefinitely.
        assert!(start.elapsed() >= Duration::from_millis(90));
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
