//! Pairing window lifecycle helpers.
//!
//! Creates and manages the initiator (and later, responder) pairing windows via
//! `WebviewWindowBuilder`. All window construction runs on the macOS main thread
//! via `AppHandle::run_on_main_thread` to avoid the macOS UI panic that occurs
//! when `WebviewWindowBuilder::build()` is called from a worker thread.

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};
use tracing::warn;

/// Label assigned to the initiator window. Used by capabilities and event
/// targeting (`emit_to(labeled("pair-initiator"), ...)`).
pub const INITIATOR_LABEL: &str = "pair-initiator";

/// Open the pair-initiator window.
///
/// If the window already exists, focuses it instead of creating a duplicate.
/// Window construction is dispatched to the macOS main thread to avoid the
/// platform's "WebView build off main thread" panic.
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
        let result = WebviewWindowBuilder::new(&app, INITIATOR_LABEL, url)
            .title("Pair with nearby device")
            .inner_size(420.0, 360.0)
            .resizable(false)
            .build();

        if let Err(e) = result {
            warn!("Failed to create pair-initiator window: {}", e);
        }
    })
}
