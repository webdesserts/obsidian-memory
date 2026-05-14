// Prevents a console window from popping up on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod daemon_task;
mod shutdown;
mod tray_status;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use daemon_task::DaemonHandle;
use shutdown::ShutdownController;
use sync_daemon::daemon::DaemonRunConfig;
use sync_daemon::pair_api::DaemonControl;
use tauri::{
    Manager,
    menu::{MenuBuilder, MenuItemBuilder, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};
use tokio::sync::Mutex;
use tracing::{info, warn};

/// Managed state wrapper so the Quit handler can take ownership of ShutdownController.
type ShutdownState = Arc<Mutex<Option<ShutdownController>>>;

/// Managed state for the daemon control handle, used by Tauri commands to send
/// commands to the running daemon (pairing, discovery, etc.).
///
/// `Arc` because Tauri managed state requires `Send + Sync + 'static`, and
/// `DaemonControl` holds non-`Clone` channels that can't be wrapped in plain `Mutex`
/// without losing the ability to clone receivers. The inner fields use tokio's own
/// thread-safe channel primitives.
pub type ControlState = Arc<DaemonControl>;

/// The port the embedded iroh relay listens on.
const RELAY_PORT: u16 = 3340;

/// Detect the machine's primary LAN IP address for advertising to peers.
///
/// Returns `None` if detection fails (e.g. no network interface, VPN-only config).
/// The relay still starts and binds to 0.0.0.0; it just won't advertise a reachable
/// URL. A warning is logged once at startup.
///
/// On machines with multiple LAN interfaces (Wi-Fi + ethernet, VPN active), this
/// picks the default route's interface — usually the right one. A manual override
/// will be available in Phase 6 via a network-interface picker in the settings UI.
fn detect_lan_ip() -> Option<std::net::IpAddr> {
    local_ip_address::local_ip().ok()
}

fn main() -> Result<()> {
    // Read the vault path from the environment — the only supported configuration
    // surface in Phase 1. Phase 6 adds a vault-picker UI.
    //
    // A CLI --vault flag is deliberately avoided: `tauri-cli` does not forward
    // trailing arguments to the bundled binary in `tauri dev` mode, which would
    // silently ignore the flag in development.
    let vault_path = match std::env::var("OBSIDIAN_MEMORY_VAULT") {
        Ok(v) if !v.is_empty() => std::path::PathBuf::from(v),
        _ => {
            eprintln!("error: OBSIDIAN_MEMORY_VAULT is not set");
            eprintln!("Set it to the path of your Obsidian vault, e.g.:");
            eprintln!("  OBSIDIAN_MEMORY_VAULT=~/notes cargo tauri dev");
            std::process::exit(1);
        }
    };

    memory_common::init_tracing(false, "desktop");

    info!("Starting Obsidian Memory desktop app");
    info!("Vault: {:?}", vault_path);

    // Detect the LAN IP for the relay's advertised URL. The relay binds to
    // 0.0.0.0:RELAY_PORT (all interfaces) but peers need a dialable address.
    let advertised_relay_url = match detect_lan_ip() {
        Some(ip) => {
            let url = format!("http://{}:{}/", ip, RELAY_PORT);
            info!("Relay will advertise LAN URL: {}", url);
            Some(url)
        }
        None => {
            warn!(
                "LAN IP detection failed — relay will start but advertise 0.0.0.0:{}. \
                 Peers on the LAN won't be able to reach the relay. \
                 Check your network connection.",
                RELAY_PORT
            );
            None
        }
    };

    let daemon_config = DaemonRunConfig {
        vault: vault_path,
        identity_key: None,
        health_listen: Some("127.0.0.1:8081".to_string()),
        relay_listen: Some(format!("0.0.0.0:{}", RELAY_PORT)),
        advertised_relay_url,
    };

    tauri::Builder::default()
        .setup(move |app| {
            // Tray-only mode: suppress the dock icon so the app lives entirely
            // in the menu bar. `"windows": []` in tauri.conf.json removes the
            // main window; this call removes the Dock icon.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let handle = app.handle().clone();

            // Spawn the sync daemon and block until startup completes. Returns
            // a DaemonHandle carrying the control, cancellation token, and done_rx.
            let daemon = DaemonHandle::spawn(daemon_config, handle.clone());

            // Decompose the handle so we can move each piece to its owner:
            // - `control` → Tauri managed state (for command handlers)
            // - `token` + `done_rx` → ShutdownController (for Quit handler)
            let (token, control, done_rx) = daemon.into_parts();

            // Status receiver cloned before control moves into Arc.
            let status_rx = control.status_rx.clone();

            // Store the DaemonControl in managed state so Tauri command handlers
            // can access it via `State<ControlState>`.
            let control: ControlState = Arc::new(control);
            app.manage(control);

            // Shutdown controller — taken once by the Quit handler.
            let shutdown = ShutdownController { token, done_rx };
            let shutdown_state: ShutdownState = Arc::new(Mutex::new(Some(shutdown)));
            app.manage(shutdown_state);

            // Build the tray menu with cached item handles so the status driver
            // can update text in-place via MenuItem::set_text.
            let status_item = MenuItemBuilder::new("Status: Connecting…")
                .id("status")
                .enabled(false) // display-only label, not interactive
                .build(app)?;

            let pair_item = MenuItemBuilder::new("Pair with nearby device…")
                .id("pair")
                .build(app)?;

            let menu = MenuBuilder::new(app)
                .items(&[
                    &status_item,
                    &PredefinedMenuItem::separator(app)?,
                    &pair_item,
                    &PredefinedMenuItem::separator(app)?,
                    &MenuItemBuilder::new("Quit").id("quit").build(app)?,
                ])
                .build()?;

            let icon = tauri::include_image!("icons/32x32.png");

            let app_for_events = handle.clone();
            TrayIconBuilder::new()
                .icon(icon)
                .menu(&menu)
                .on_menu_event(move |_tray, event| {
                    match event.id().as_ref() {
                        "quit" => {
                            let app = app_for_events.clone();
                            tauri::async_runtime::spawn(async move {
                                let state = app.state::<ShutdownState>();
                                let shutdown = state.lock().await.take();
                                if let Some(s) = shutdown {
                                    s.shutdown(Duration::from_secs(5)).await;
                                }
                                app.exit(0);
                            });
                        }
                        "pair" => {
                            // Wired in commit 4 — opens the initiator window.
                            warn!("Pair menu clicked; initiator window not yet wired");
                        }
                        _ => {}
                    }
                })
                .on_tray_icon_event(|_tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        // Left-click on the tray icon — no action in Phase 1.5.
                        // Phase 6 will open the status popup here.
                    }
                })
                .build(app)?;

            // Start the tray status driver. It subscribes to status_rx and calls
            // MenuItem::set_text on the status_item handle whenever the daemon
            // status changes, without rebuilding the entire menu.
            let handles = tray_status::TrayMenuHandles {
                status_item,
                pair_item,
            };
            tray_status::start(handle.clone(), handles, status_rx);

            Ok(())
        })
        .run(tauri::generate_context!())
        .map_err(|e| anyhow::anyhow!("Tauri application error: {e}"))?;

    Ok(())
}
