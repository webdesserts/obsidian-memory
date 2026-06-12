//! Tauri command handlers bridging the pairing UI to the daemon event loop.
//!
//! Each command sends a `DaemonCommand` to the running daemon via
//! `DaemonControl.command_tx` and awaits the reply. Commands are registered in
//! `main.rs` via `.invoke_handler(tauri::generate_handler![...])`.
//!
//! The daemon-side implementations of `StartDiscovery` and `SubmitCode` live in
//! `crates/sync-daemon/src/daemon.rs` (commit 4).

use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

use sync_daemon::pair_api::DaemonCommand;

use crate::ControlState;

/// Send a oneshot `DaemonCommand` and await its reply.
///
/// `make_cmd` receives the reply sender and wraps it into the appropriate
/// `DaemonCommand` variant. Returns an error string if the daemon's command
/// channel is closed (not running) or if the reply channel is dropped before
/// the daemon responds (disconnected mid-flight).
async fn dispatch<T>(
    control: &ControlState,
    make_cmd: impl FnOnce(oneshot::Sender<T>) -> DaemonCommand,
) -> Result<T, String> {
    let (reply_tx, reply_rx) = oneshot::channel();
    control
        .command_tx
        .send(make_cmd(reply_tx))
        .map_err(|_| "Daemon is not running".to_string())?;
    reply_rx
        .await
        .map_err(|_| "Daemon disconnected before replying".to_string())
}

/// Payload emitted to the pair-initiator window for each discovered mesh.
#[derive(Serialize, Clone)]
pub struct MeshDiscoveredPayload {
    pub vault_id: String,
    pub mesh_name: String,
    pub online_count: usize,
}

/// Result returned by `submit_pair_code` on success.
#[derive(Serialize)]
pub struct PairSuccessResult {
    pub device_name: String,
}

/// Start mDNS discovery and stream discovered meshes to the pair-initiator window.
///
/// Sends `DaemonCommand::StartDiscovery` to the daemon. The daemon runs mDNS
/// for up to 10 seconds, forwarding each `DiscoveredMesh` back through the
/// reply channel. For each mesh, this command emits a `pair://mesh-discovered`
/// event to the `pair-initiator` window. After the scan completes (channel
/// closed), it emits `pair://discovery-finished`.
#[tauri::command]
pub async fn start_pair_discovery(
    app: AppHandle,
    control: State<'_, ControlState>,
) -> Result<(), String> {
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel();

    control
        .command_tx
        .send(DaemonCommand::StartDiscovery { reply: reply_tx })
        .map_err(|_| "Daemon is not running".to_string())?;

    // Spawn a task to forward mesh events to the pair-initiator window as
    // Tauri events, then emit discovery-finished when the daemon closes the channel.
    tauri::async_runtime::spawn(async move {
        while let Some(mesh) = reply_rx.recv().await {
            let payload = MeshDiscoveredPayload {
                vault_id: mesh.vault_id,
                mesh_name: mesh.mesh_name,
                online_count: mesh.online_count,
            };
            if let Err(e) = app.emit_to(
                tauri::EventTarget::labeled("pair-initiator"),
                "pair://mesh-discovered",
                payload,
            ) {
                warn!("Failed to emit mesh-discovered event: {}", e);
            }
        }
        // Channel closed — scan finished. Notify the window.
        if let Err(e) = app.emit_to(
            tauri::EventTarget::labeled("pair-initiator"),
            "pair://discovery-finished",
            (),
        ) {
            warn!("Failed to emit discovery-finished event: {}", e);
        }
    });

    Ok(())
}

/// Connect to the selected mesh and park the QUIC connection, triggering the
/// responder to generate + display its 6-digit code.
///
/// Step 1 of the two-step GUI pairing flow. Sends `DaemonCommand::RequestPairing`
/// and waits until the connection is established and the responder is showing its
/// code. On success, returns the responder's device name so the UI can show
/// "Enter the code shown on <device>". On failure, returns an error string so the
/// UI can re-enable the Request button and let the user retry.
#[tauri::command]
pub async fn request_pairing(
    vault_id: String,
    control: State<'_, ControlState>,
) -> Result<PairSuccessResult, String> {
    let device_name =
        dispatch(&control, |reply| DaemonCommand::RequestPairing { vault_id, reply }).await??;
    Ok(PairSuccessResult { device_name })
}

/// Submit the 6-digit pairing code for the currently-selected mesh.
///
/// Step 2 of the two-step GUI pairing flow. Requires a prior `request_pairing`
/// call to have succeeded. Sends `DaemonCommand::SubmitCode` and waits for the
/// daemon to complete the HMAC exchange. Returns the paired device's name on
/// success, or an error string on failure.
#[tauri::command]
pub async fn submit_pair_code(
    vault_id: String,
    code: String,
    control: State<'_, ControlState>,
) -> Result<PairSuccessResult, String> {
    let device_name =
        dispatch(&control, |reply| DaemonCommand::SubmitCode { code, vault_id, reply }).await??;
    Ok(PairSuccessResult { device_name })
}

/// Cancel the active initiator pairing session. Idempotent.
///
/// Sends `DaemonCommand::CancelInitiate`. Returns `Ok(())` whether or not a
/// session is currently active — handles the race where the user clicks Cancel
/// after pairing has already completed but the window is still closing.
#[tauri::command]
pub async fn cancel_pair_discovery(control: State<'_, ControlState>) -> Result<(), String> {
    dispatch(&control, |reply| DaemonCommand::CancelInitiate { reply }).await
}

/// Reject the currently-active inbound pairing request. Idempotent.
///
/// Sends `DaemonCommand::RejectInbound`. The daemon drops `active_pairing`,
/// which closes the pairing handler's reply channel and surfaces to the
/// requesting peer as a failed pairing. Returns `Ok(())` whether or not an
/// inbound session is currently active — the responder window can call this
/// safely after the daemon has already failed or completed the exchange.
#[tauri::command]
pub async fn reject_inbound_pair(control: State<'_, ControlState>) -> Result<(), String> {
    dispatch(&control, |reply| DaemonCommand::RejectInbound { reply }).await
}
