// Prevents a console window from popping up on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod daemon_task;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use daemon_task::DaemonHandle;
use sync_daemon::daemon::DaemonRunConfig;
use tauri::{
    Manager,
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};
use tokio::sync::Mutex;
use tracing::{info, warn};

/// Managed state wrapper so the Quit handler can take ownership of DaemonHandle.
///
/// DaemonHandle::shutdown() consumes self to enforce single-call semantics, so
/// the handle is wrapped in Option and taken on the first Quit event.
type DaemonState = Arc<Mutex<Option<DaemonHandle>>>;

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
            // App::set_activation_policy (on &mut App in setup()) returns (). The
            // AppHandle variant returns Result, but the App variant used here does not.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let handle = app.handle().clone();

            // Spawn the sync daemon and wire up the failure watchdog.
            let daemon = DaemonHandle::spawn(daemon_config, handle.clone());

            // Wrap in Arc<Mutex<Option<...>>> so the Quit handler can take
            // ownership of DaemonHandle when calling shutdown() (which consumes self).
            let daemon_state: DaemonState = Arc::new(Mutex::new(Some(daemon)));
            app.manage(daemon_state);

            // Build the tray icon and menu.
            let menu = tauri::menu::MenuBuilder::new(app)
                .text("status", "Status: Running")
                .separator()
                .text("quit", "Quit")
                .build()?;

            // Load the tray icon embedded at compile time via Tauri's include_image!
            // macro. The 32x32.png is the standard macOS menu bar icon size.
            let icon = tauri::include_image!("icons/32x32.png");

            TrayIconBuilder::new()
                .icon(icon)
                .menu(&menu)
                .on_menu_event(move |app, event| {
                    if event.id().as_ref() == "quit" {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let state = app.state::<DaemonState>();
                            let daemon = state.lock().await.take();
                            if let Some(d) = daemon {
                                d.shutdown(Duration::from_secs(5)).await;
                            }
                            app.exit(0);
                        });
                    }
                })
                .on_tray_icon_event(|_tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        // Left-click on the tray icon — no action in Phase 1.
                        // Phase 6 will open the status popup here.
                    }
                })
                .build(app)?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .map_err(|e| anyhow::anyhow!("Tauri application error: {e}"))?;

    Ok(())
}
