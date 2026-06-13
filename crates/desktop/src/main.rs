// Prevents a console window from popping up on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod daemon_task;
mod notification;
mod pair_events;
mod pair_window;
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

/// The port the local health endpoint binds on. 8081 is the conventional default
/// but is commonly taken (e.g. llama-swap), so we standardize on 8082 across all
/// machines. Make this env-configurable later if a machine ever needs to differ.
const HEALTH_PORT: u16 = 8082;

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

/// Interface-name prefixes that identify a real physical NIC. Selection biases
/// toward these so a routable LAN address on Wi-Fi/ethernet beats one that happens
/// to sit on a virtual interface.
const REAL_NIC_PREFIXES: &[&str] = &["en", "eth"];

/// Interface-name prefixes for virtual / bridge / tunnel interfaces (macOS-flavored).
/// These are deprioritized: macOS auto-creates `bridge0` for iPhone-USB / hotspot,
/// `utun*`/`awdl*` for VPN/AirDrop, etc., and advertising an address on one of them
/// leaves peers unable to route to it.
const VIRTUAL_IFACE_PREFIXES: &[&str] = &[
    "bridge", "utun", "awdl", "llw", "gif", "stf", "ap", "anpi", "tun", "tap", "vnic",
    "vmnet", "bond", "feth", "docker", "veth",
];

/// Choose the best LAN address to advertise from a list of (interface_name, IpAddr)
/// candidates (as returned by `local_ip_address::list_afinet_netifas`).
///
/// Selection rules, in order:
///   1. Reject loopback (127/8, ::1) and link-local (169.254/16, fe80::/10).
///   2. Prefer routable private IPv4 (RFC1918) over anything else.
///   3. Among equally-eligible addresses, prefer a real physical interface
///      (en*/eth*) over bridge/virtual/tunnel interfaces (bridge*/utun*/awdl*/...).
///   4. Prefer IPv4 over IPv6.
///
/// On equal score the candidate appearing first in the list wins, so the result is
/// deterministic across runs. Returns `None` if no acceptable address exists (e.g.
/// fully offline).
fn select_advertise_ip(candidates: &[(String, std::net::IpAddr)]) -> Option<std::net::IpAddr> {
    let mut best: Option<(i32, std::net::IpAddr)> = None;

    for (name, addr) in candidates {
        let Some(score) = score_candidate(name, addr) else {
            continue; // rejected (loopback / link-local)
        };

        // Strict `>` so the FIRST candidate to reach a given score is retained —
        // first-in-list wins on ties (deterministic). `max_by_key` would keep the
        // last equal element instead.
        if best.is_none_or(|(best_score, _)| score > best_score) {
            best = Some((score, *addr));
        }
    }

    best.map(|(_, addr)| addr)
}

/// Score a single candidate, or `None` if it must be rejected outright.
/// Higher score = better. See `select_advertise_ip` for the ranking rationale.
fn score_candidate(name: &str, addr: &std::net::IpAddr) -> Option<i32> {
    let mut score = 0;

    match addr {
        std::net::IpAddr::V4(ip) => {
            if ip.is_loopback() || ip.is_link_local() {
                return None;
            }
            if ip.is_private() {
                score += 100;
            }
            score += 1; // prefer IPv4 over IPv6
        }
        std::net::IpAddr::V6(ip) => {
            // `Ipv6Addr::is_unicast_link_local` is unstable, so match fe80::/10 by
            // inspecting the first 10 bits manually.
            let is_link_local = (ip.segments()[0] & 0xffc0) == 0xfe80;
            if ip.is_loopback() || is_link_local {
                return None;
            }
        }
    }

    if REAL_NIC_PREFIXES.iter().any(|p| name.starts_with(p)) {
        score += 10;
    } else if VIRTUAL_IFACE_PREFIXES.iter().any(|p| name.starts_with(p)) {
        score -= 10;
    }

    Some(score)
}

/// Resolve the URL to advertise for this node's embedded relay.
///
/// Priority (highest to lowest):
/// 1. `OBSIDIAN_MEMORY_RELAY_URL` env var — explicit stable URL (e.g. umbra running
///    with a public hostname). Takes precedence over everything when set and non-empty.
/// 2. Detected LAN IP — the default on a machine without a stable public URL.
/// 3. `None` — relay starts but can't be reached by off-LAN peers.
///
/// Extracted as a pure function so the logic is testable without launching Tauri.
fn resolve_advertised_relay_url(
    env_val: Option<String>,
    detected: Option<std::net::IpAddr>,
) -> Option<String> {
    if let Some(url) = env_val.filter(|v| !v.is_empty()) {
        return Some(url);
    }
    detected.map(|ip| format!("http://{}:{}/", ip, RELAY_PORT))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    /// Build a candidate `(interface_name, IpAddr)` pair from an IPv4 octet quad.
    fn v4(name: &str, a: u8, b: u8, c: u8, d: u8) -> (String, IpAddr) {
        (name.to_string(), IpAddr::V4(Ipv4Addr::new(a, b, c, d)))
    }

    /// Build a candidate `(interface_name, IpAddr)` pair from IPv6 segments.
    fn v6(name: &str, segments: [u16; 8]) -> (String, IpAddr) {
        (name.to_string(), IpAddr::V6(Ipv6Addr::from(segments)))
    }

    #[test]
    fn picks_real_nic_over_link_local_bridge() {
        // The live reproduction: a macOS bridge0 holds a link-local APIPA address
        // and wins the default-route lookup, but en0 has the routable LAN address.
        let candidates = [
            v4("lo0", 127, 0, 0, 1),
            v4("bridge0", 169, 254, 112, 191),
            v4("en0", 192, 168, 68, 53),
        ];
        assert_eq!(
            select_advertise_ip(&candidates),
            Some(IpAddr::V4(Ipv4Addr::new(192, 168, 68, 53))),
        );
    }

    #[test]
    fn rejects_loopback_only() {
        let candidates = [v4("lo0", 127, 0, 0, 1)];
        assert_eq!(select_advertise_ip(&candidates), None);
    }

    #[test]
    fn rejects_link_local_only() {
        let candidates = [v4("bridge0", 169, 254, 5, 5)];
        assert_eq!(select_advertise_ip(&candidates), None);
    }

    #[test]
    fn prefers_real_nic_over_bridge_when_both_private() {
        // Even when a bridge interface hands out a private (hotspot) address, the
        // real NIC is preferred via the interface-name bias.
        let candidates = [v4("bridge0", 172, 20, 10, 2), v4("en0", 192, 168, 68, 53)];
        assert_eq!(
            select_advertise_ip(&candidates),
            Some(IpAddr::V4(Ipv4Addr::new(192, 168, 68, 53))),
        );
    }

    #[test]
    fn prefers_private_over_public() {
        // A public IPv4 on a real NIC must not beat an RFC1918 LAN address — peers
        // need the routable-on-this-LAN address, not a public one.
        let candidates = [v4("en5", 8, 8, 8, 8), v4("en0", 10, 0, 0, 4)];
        assert_eq!(
            select_advertise_ip(&candidates),
            Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 4))),
        );
    }

    #[test]
    fn prefers_ipv4_over_ipv6() {
        // Concrete inputs so the assertion pins a specific address, not a V4 pattern.
        let candidates = [
            v4("en0", 10, 0, 0, 5),
            v6("en0", [0x2001, 0x0db8, 0, 0, 0, 0, 0, 1]),
        ];
        assert_eq!(
            select_advertise_ip(&candidates),
            Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5))),
        );
    }

    #[test]
    fn rejects_ipv6_link_local() {
        // fe80::/10 is link-local and unroutable; with nothing else, selection fails.
        let candidates = [v6("en0", [0xfe80, 0, 0, 0, 0, 0, 0, 1])];
        assert_eq!(select_advertise_ip(&candidates), None);
    }

    #[test]
    fn deterministic_on_tie() {
        // Two equally-scored real-NIC private addresses: the first in the list wins,
        // proving the stable tie-break rather than relying on iteration luck.
        let candidates = [v4("en0", 192, 168, 1, 10), v4("en1", 192, 168, 1, 20)];
        assert_eq!(
            select_advertise_ip(&candidates),
            Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10))),
        );
    }

    #[test]
    fn empty_list_returns_none() {
        assert_eq!(select_advertise_ip(&[]), None);
    }

    #[test]
    fn env_override_wins_over_detected_ip() {
        let env_url = Some("http://umbra.computer:3340/".to_string());
        let detected = Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)));
        assert_eq!(
            resolve_advertised_relay_url(env_url, detected),
            Some("http://umbra.computer:3340/".to_string()),
        );
    }

    #[test]
    fn env_override_wins_even_without_detected_ip() {
        let env_url = Some("http://umbra.computer:3340/".to_string());
        assert_eq!(
            resolve_advertised_relay_url(env_url, None),
            Some("http://umbra.computer:3340/".to_string()),
        );
    }

    #[test]
    fn detected_ip_used_when_no_env_override() {
        let detected = Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)));
        assert_eq!(
            resolve_advertised_relay_url(None, detected),
            Some(format!("http://192.168.1.10:{}/", RELAY_PORT)),
        );
    }

    #[test]
    fn empty_env_var_falls_through_to_detected_ip() {
        // An empty env var is treated as unset — the user didn't intend an override.
        let detected = Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
        assert_eq!(
            resolve_advertised_relay_url(Some(String::new()), detected),
            Some(format!("http://10.0.0.1:{}/", RELAY_PORT)),
        );
    }

    #[test]
    fn none_when_no_env_and_no_detected_ip() {
        assert_eq!(resolve_advertised_relay_url(None, None), None);
    }
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

    info!("Starting Memory");
    info!("Vault: {:?}", vault_path);

    // Resolve the URL advertised to peers for this node's embedded relay.
    //
    // The env var `OBSIDIAN_MEMORY_RELAY_URL` takes precedence so stable-hostname
    // machines (e.g. umbra) can advertise a public URL instead of the detected LAN
    // IP. When unset, fall back to LAN IP detection. Log which source won.
    let env_relay_url = std::env::var("OBSIDIAN_MEMORY_RELAY_URL").ok();
    let detected_ip = detect_lan_ip();
    let advertised_relay_url = resolve_advertised_relay_url(env_relay_url.clone(), detected_ip);

    match &advertised_relay_url {
        Some(url) if env_relay_url.as_deref().is_some_and(|v| !v.is_empty()) => {
            info!("Relay will advertise URL from OBSIDIAN_MEMORY_RELAY_URL: {}", url);
        }
        Some(url) => {
            info!("Relay will advertise LAN URL: {}", url);
        }
        None => {
            warn!(
                "LAN IP detection failed — relay will start but advertise 0.0.0.0:{}. \
                 Peers on the LAN won't be able to reach the relay. \
                 Check your network connection.",
                RELAY_PORT
            );
        }
    }

    let daemon_config = DaemonRunConfig {
        vault: vault_path,
        identity_key: None,
        health_listen: Some(format!("127.0.0.1:{}", HEALTH_PORT)),
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

            // Receivers cloned/extracted before control moves into Arc.
            //
            // `status_rx` is a watch::Receiver (Clone), but `pairing_rx` is a
            // broadcast::Receiver (not Clone). We resubscribe from the
            // broadcast sender via `Receiver::resubscribe` to obtain an
            // independent stream of pairing events for the consumer task.
            let status_rx = control.status_rx.clone();
            let pairing_rx = control.pairing_rx.resubscribe();

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
                .on_menu_event(move |_tray, event| match event.id().as_ref() {
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
                        if let Err(e) = pair_window::open_initiator(&app_for_events) {
                            warn!("Failed to open pair initiator window: {}", e);
                        }
                    }
                    _ => {}
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

            // Start the pairing-events consumer. Translates daemon-side
            // PairingUiEvents into the responder window + macOS notification.
            pair_events::start(handle.clone(), pairing_rx);

            Ok(())
        })
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(tauri::generate_handler![
            commands::start_pair_discovery,
            commands::request_pairing,
            commands::submit_pair_code,
            commands::cancel_pair_discovery,
            commands::reject_inbound_pair,
        ])
        .build(tauri::generate_context!())
        .map_err(|e| anyhow::anyhow!("Tauri application error: {e}"))?
        .run(|_app, event| {
            // Tray-only app: closing a transient window (e.g. the pair dialog)
            // must NOT terminate the process. Tauri fires ExitRequested with
            // code=None whenever the last window closes; only honor it when
            // code is Some (the Quit menu calls `app.exit(0)`).
            if let tauri::RunEvent::ExitRequested { code: None, api, .. } = &event {
                api.prevent_exit();
            }
        });

    Ok(())
}
