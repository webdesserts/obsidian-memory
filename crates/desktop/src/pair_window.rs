//! Pairing window lifecycle helpers.
//!
//! Creates and manages the initiator (and later, responder) pairing windows via
//! `WebviewWindowBuilder`. All window construction runs on the macOS main thread
//! via `AppHandle::run_on_main_thread` to avoid the macOS UI panic that occurs
//! when `WebviewWindowBuilder::build()` is called from a worker thread.

use sync_daemon::pair_api::DaemonCommand;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tokio::sync::oneshot;
use tracing::warn;

use crate::ControlState;

/// Label assigned to the initiator window. Used by capabilities and event
/// targeting (`emit_to(labeled("pair-initiator"), ...)`).
pub const INITIATOR_LABEL: &str = "pair-initiator";

/// Open the pair-initiator window.
///
/// If the window already exists, focuses it instead of creating a duplicate.
/// Window construction is dispatched to the macOS main thread to avoid the
/// platform's "WebView build off main thread" panic.
///
/// Registers a close handler that issues `DaemonCommand::CancelInitiate` so the
/// daemon drops its in-flight initiator session when the user closes the window
/// via the X / Cmd+W rather than the in-window Cancel button. The daemon-side
/// command is idempotent, so it is safe whether or not an initiator session is
/// actually active.
pub fn open_initiator(app: &AppHandle) -> tauri::Result<()> {
    let app = app.clone();
    app.clone().run_on_main_thread(move || {
        if let Some(existing) = app.get_webview_window(INITIATOR_LABEL) {
            if let Err(e) = existing.set_focus() {
                warn!("Failed to focus existing pair-initiator window: {}", e);
            }
            return;
        }

        let url = WebviewUrl::App("windows/pair-initiator.html".into());
        let build_result = WebviewWindowBuilder::new(&app, INITIATOR_LABEL, url)
            .title("Pair with nearby device")
            .inner_size(420.0, 360.0)
            .resizable(false)
            .build();

        let window = match build_result {
            Ok(w) => w,
            Err(e) => {
                warn!("Failed to create pair-initiator window: {}", e);
                return;
            }
        };

        let app_for_close = app.clone();
        window.on_window_event(move |event| {
            if matches!(event, WindowEvent::CloseRequested { .. }) {
                send_cancel_initiate(&app_for_close);
            }
        });
    })
}

/// Fire `DaemonCommand::CancelInitiate` so the daemon drops its in-flight
/// initiator session. The reply oneshot is awaited on a background task and
/// discarded — the close handler returns immediately so window teardown isn't
/// blocked on the daemon's reply.
fn send_cancel_initiate(app: &AppHandle) {
    let control = app.state::<ControlState>();
    let command_tx = control.command_tx.clone();
    let (reply_tx, reply_rx) = oneshot::channel::<()>();

    if let Err(e) = command_tx.send(DaemonCommand::CancelInitiate { reply: reply_tx }) {
        warn!("Failed to send CancelInitiate on initiator window close: {}", e);
        return;
    }

    tauri::async_runtime::spawn(async move {
        let _ = reply_rx.await;
    });
}
