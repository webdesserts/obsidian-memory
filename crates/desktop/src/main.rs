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
use tracing::info;

/// Managed state wrapper so the Quit handler can take ownership of DaemonHandle.
///
/// DaemonHandle::shutdown() consumes self to enforce single-call semantics, so
/// the handle is wrapped in Option and taken on the first Quit event.
type DaemonState = Arc<Mutex<Option<DaemonHandle>>>;

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

    let daemon_config = DaemonRunConfig {
        vault: vault_path,
        identity_key: None,
        health_listen: Some("127.0.0.1:8081".to_string()),
        relay_listen: None,
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
