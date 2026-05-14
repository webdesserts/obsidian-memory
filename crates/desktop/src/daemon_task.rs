//! Cancellation-token-aware daemon spawn helper.
//!
//! Spawns `sync_daemon::daemon::run_with_shutdown_controlled` as a tokio task
//! inside Tauri's runtime, returning a `DaemonHandle` that carries both the
//! shutdown token and a `DaemonControl` for the tray status driver and pairing
//! commands.
//!
//! The watchdog task exits the application if the daemon task fails or panics,
//! surfacing daemon errors (e.g. "another instance already holds the lock") to
//! the user as a visible app exit rather than a silent zombie tray icon.

use sync_daemon::daemon::{DaemonRunConfig, run_with_shutdown_controlled};
use sync_daemon::pair_api::DaemonControl;
use tauri::AppHandle;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

/// Handle to the running daemon task, accessible to the Quit handler.
pub struct DaemonHandle {
    pub token: CancellationToken,
    /// Live daemon control handle — subscribe to status, send pairing commands.
    pub control: DaemonControl,
    /// Receives a signal when the watchdog has observed the daemon task exit.
    done_rx: oneshot::Receiver<()>,
}

impl DaemonHandle {
    /// Spawn the daemon task and start the watchdog.
    ///
    /// Calls `run_with_shutdown_controlled` so that startup completes before
    /// returning, giving the caller a `DaemonControl` immediately after this
    /// function returns. The watchdog holds sole ownership of the `JoinHandle`
    /// to prevent a double-take race with `shutdown()`. If startup fails,
    /// `app.exit(1)` is called and a stub `DaemonControl` is returned so the
    /// process exits cleanly.
    pub fn spawn(config: DaemonRunConfig, app: AppHandle) -> Self {
        let token = CancellationToken::new();
        let (done_tx, done_rx) = oneshot::channel::<()>();

        // `std::sync::mpsc` lets us block the sync `setup()` thread while
        // startup runs asynchronously in Tauri's Tokio runtime. Startup
        // typically takes <1 second (lock + vault load + relay start).
        let (control_tx, control_rx) = std::sync::mpsc::channel::<DaemonControl>();

        let token_clone = token.clone();
        tauri::async_runtime::spawn(async move {
            // Run the startup phase. Failures are fatal.
            let (control, join_handle) = match run_with_shutdown_controlled(config, token_clone).await {
                Ok(pair) => pair,
                Err(e) => {
                    error!("Daemon startup failed: {e:#}");
                    app.exit(1);
                    // Send a stub so the receiver unblocks; the process is exiting.
                    let _ = control_tx.send(make_stub_control());
                    let _ = done_tx.send(());
                    return;
                }
            };

            // Hand the control back to the spawner before the event loop starts.
            let _ = control_tx.send(control);

            // Watchdog: wait for the event loop to exit.
            match join_handle.await {
                Ok(Ok(())) => {
                    info!("Daemon task exited cleanly");
                }
                Ok(Err(e)) => {
                    error!("Daemon exited with error: {e:#}");
                    app.exit(1);
                }
                Err(e) => {
                    error!("Daemon task panicked: {e}");
                    app.exit(1);
                }
            }
            let _ = done_tx.send(());
        });

        // Block the setup() thread until startup is complete.
        let control = control_rx
            .recv()
            .unwrap_or_else(|_| make_stub_control());

        DaemonHandle { token, control, done_rx }
    }

    /// Decompose this handle into its constituent parts.
    ///
    /// Used by `main.rs` to move `DaemonControl` into managed state independently
    /// from the cancellation token (used only by the Quit handler).
    pub fn into_parts(self) -> (CancellationToken, DaemonControl, oneshot::Receiver<()>) {
        (self.token, self.control, self.done_rx)
    }

}

/// Build a stub `DaemonControl` for failure scenarios.
///
/// All channels are immediately disconnected so any subscriber sees EOF.
fn make_stub_control() -> DaemonControl {
    use sync_daemon::pair_api::DaemonStatus;
    use tokio::sync::{broadcast, mpsc, watch};
    let (_, status_rx) = watch::channel(DaemonStatus::initial());
    // Capacity matches PAIRING_BROADCAST_CAPACITY in pair_api.rs.
    let (_, pairing_rx) = broadcast::channel(16);
    let (command_tx, _) = mpsc::unbounded_channel();
    DaemonControl {
        status_rx,
        pairing_rx,
        command_tx,
    }
}
