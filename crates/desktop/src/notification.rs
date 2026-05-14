//! macOS notification dispatch for pairing events.
//!
//! Wraps `tauri-plugin-notification` so the rest of the codebase can call a
//! single function and not worry about plugin availability or permission state.
//! Notifications are best-effort UX glue — if dispatch fails for any reason
//! (permission denied, plugin not registered, unsigned bundle on a strict
//! system), the responder window is still the load-bearing surface and the
//! user can still complete pairing.

use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;
use tracing::warn;

/// Post a macOS notification announcing an inbound pairing request.
///
/// The body includes the displayed code so the user can read it without
/// opening the responder window. Failure is logged at WARN and otherwise
/// ignored — the responder window remains the authoritative UI surface for
/// the pairing exchange.
pub fn post_pair_request(app: &AppHandle, device_name: &str, code: &str) {
    let body = format!("{device_name} wants to pair. Enter code: {code}");
    if let Err(e) = app
        .notification()
        .builder()
        .title("Pair request")
        .body(body)
        .show()
    {
        warn!("Failed to post macOS notification for pair request: {}", e);
    }
}
