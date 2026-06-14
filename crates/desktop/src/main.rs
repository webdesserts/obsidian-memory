// Prevents a console window from popping up on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app_settings;
mod commands;
mod daemon_task;
mod notification;
mod pair_events;
mod pair_window;
mod settings_window;
mod shutdown;
mod tray_status;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use app_settings::{AppSettings, resolve_vault_path};
use daemon_task::DaemonHandle;
use shutdown::ShutdownController;
use sync_daemon::daemon::DaemonRunConfig;
use sync_daemon::pair_api::DaemonControl;
use tauri::{
    Manager,
    menu::{CheckMenuItemBuilder, MenuBuilder, MenuItemBuilder, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};
use tauri_plugin_autostart::ManagerExt;
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
/// Enumerates every network interface and selects a routable address via
/// [`select_advertise_ip`], rejecting loopback and link-local addresses and
/// preferring an RFC1918 address on a real NIC. This avoids advertising an
/// unroutable APIPA address that a macOS auto-created `bridge0` (iPhone-USB /
/// hotspot) can otherwise win via the default-route lookup.
///
/// Returns `None` if enumeration fails or no routable address exists (e.g. fully
/// offline). The relay still starts and binds to 0.0.0.0; it just won't advertise a
/// reachable URL. The chosen interface and address are logged at INFO; the no-address
/// paths log a WARN. A manual override is available now via `OBSIDIAN_MEMORY_RELAY_URL`,
/// and a network-interface picker is planned for the settings UI in Phase 6.
fn detect_lan_ip() -> Option<std::net::IpAddr> {
    match local_ip_address::list_afinet_netifas() {
        Ok(ifaces) => {
            let chosen = select_advertise_ip(&ifaces);
            match &chosen {
                Some(ip) => {
                    let name = ifaces
                        .iter()
                        .find(|(_, a)| a == ip)
                        .map(|(n, _)| n.as_str())
                        .unwrap_or("?");
                    info!(
                        "Selected LAN address {ip} on interface '{name}' \
                         (rejected link-local/loopback, preferred routable NIC)"
                    );
                }
                None => warn!(
                    "No routable LAN address found among {} interfaces; \
                     relay will advertise 0.0.0.0. Candidates were: {:?}",
                    ifaces.len(),
                    ifaces
                ),
            }
            chosen
        }
        Err(e) => {
            warn!("Interface enumeration failed ({e}); relay won't advertise a reachable LAN URL");
            None
        }
    }
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

/// Pick the explicitly-configured relay URL, preferring the env var over the
/// stored value.
///
/// The env var (`OBSIDIAN_MEMORY_RELAY_URL`) is an override that always wins; the
/// stored value (set via the settings panel) is the fallback used by autostarted
/// instances that launch without shell env vars. Both inputs are expected to be
/// already empty-filtered by the caller. The result feeds the first argument of
/// [`resolve_advertised_relay_url`], so a detected LAN IP still backstops both.
///
/// Extracted as a pure function so the precedence is testable without launching Tauri.
fn resolve_configured_relay_url(
    env_relay_url: Option<String>,
    stored_relay_url: Option<String>,
) -> Option<String> {
    env_relay_url.or(stored_relay_url)
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

    #[test]
    fn configured_relay_env_wins_over_stored() {
        assert_eq!(
            resolve_configured_relay_url(
                Some("http://umbra.computer:3340/".to_string()),
                Some("http://stored.example/".to_string()),
            ),
            Some("http://umbra.computer:3340/".to_string()),
        );
    }

    #[test]
    fn configured_relay_stored_used_when_no_env() {
        assert_eq!(
            resolve_configured_relay_url(None, Some("http://stored.example/".to_string())),
            Some("http://stored.example/".to_string()),
        );
    }

    #[test]
    fn configured_relay_none_when_neither_set() {
        assert_eq!(resolve_configured_relay_url(None, None), None);
    }

    #[test]
    fn stored_relay_url_wins_over_detected_ip() {
        // Mirrors `env_override_wins_over_detected_ip`: a relay URL configured via the
        // settings panel (stored, no env override) must beat the detected LAN IP.
        let stored = resolve_configured_relay_url(None, Some("http://stored.example/".to_string()));
        let detected = Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)));
        assert_eq!(
            resolve_advertised_relay_url(stored, detected),
            Some("http://stored.example/".to_string()),
        );
    }
}

fn main() -> Result<()> {
    memory_common::init_tracing(false, "desktop");

    info!("Starting Memory");

    tauri::Builder::default()
        // Plugins must be registered before setup() so that their APIs are
        // available inside the setup hook via app.store() and app.autolaunch().
        .plugin(tauri_plugin_store::Builder::default().build())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None, // no extra args — vault path is read from the store, not CLI
        ))
        .setup(move |app| {
            // --- Vault path resolution (amendment S2: daemon_config built here) ---
            //
            // Priority: OBSIDIAN_MEMORY_VAULT env var > stored path from
            // app-settings.json. The env var is always written back to the store so
            // that autostarted instances (which launch without shell env vars) find
            // the vault at the same location (amendment B2).
            //
            // A CLI --vault flag is deliberately avoided: `tauri-cli` does not
            // forward trailing arguments to the bundled binary in `tauri dev` mode,
            // which would silently ignore the flag in development.
            let mut settings = AppSettings::load(app.handle())?;
            let env_vault = std::env::var("OBSIDIAN_MEMORY_VAULT").ok();
            let vault_path = resolve_vault_path(
                env_vault.as_deref(),
                settings.vault_path().and_then(|p| p.to_str()),
            )
            .ok_or_else(|| {
                anyhow!(
                    "OBSIDIAN_MEMORY_VAULT is not set and no vault path is stored.\n\
                     Set it to the path of your Obsidian vault, e.g.:\n\
                     \x20 OBSIDIAN_MEMORY_VAULT=~/notes cargo tauri dev"
                )
            })?;

            info!("Vault: {:?}", vault_path);

            // Unconditionally refresh the stored vault path whenever the env var is set
            // (amendment B2). This keeps the stored path current if the user moves their
            // vault and reruns with the updated env var, so a future autostarted launch
            // (which has no env var) still finds the right location.
            if env_vault.as_deref().is_some_and(|v| !v.is_empty()) {
                settings.set_vault_path(vault_path.clone());
                if let Err(e) = settings.save(app.handle()) {
                    warn!("Failed to persist vault path to app settings: {e}");
                }
            }

            // Resolve the URL advertised to peers for this node's embedded relay.
            //
            // Priority: OBSIDIAN_MEMORY_RELAY_URL env var > relay URL stored in
            // app-settings.json (set via the settings panel) > detected LAN IP. The
            // env var takes precedence so stable-hostname machines (e.g. umbra) can
            // advertise a public URL; the stored value lets autostarted instances —
            // which launch without shell env vars — still advertise the configured URL.
            let env_relay_url = std::env::var("OBSIDIAN_MEMORY_RELAY_URL")
                .ok()
                .filter(|v| !v.is_empty());
            let stored_relay_url = settings.relay_url().map(String::from);
            let configured_relay_url =
                resolve_configured_relay_url(env_relay_url.clone(), stored_relay_url.clone());
            let detected_ip = detect_lan_ip();
            let advertised_relay_url =
                resolve_advertised_relay_url(configured_relay_url, detected_ip);

            match &advertised_relay_url {
                Some(url) if env_relay_url.is_some() => {
                    info!("Relay will advertise URL from OBSIDIAN_MEMORY_RELAY_URL: {}", url);
                }
                Some(url) if stored_relay_url.is_some() => {
                    info!("Relay will advertise URL from stored settings: {}", url);
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

            // Read the autostart state from the OS (the LaunchAgent plist) to
            // reflect what was previously registered, not what the store says.
            // The store is written only on change; the plist is ground truth.
            let is_autostart_enabled = app.autolaunch().is_enabled().unwrap_or(false);

            let autostart_item = CheckMenuItemBuilder::new("Launch at Login")
                .id("toggle-autostart")
                .checked(is_autostart_enabled)
                .build(app)?;

            let pair_item = MenuItemBuilder::new("Pair with nearby device…")
                .id("pair")
                .build(app)?;

            let settings_item = MenuItemBuilder::new("Settings…").id("settings").build(app)?;

            // Menu order (D1): [status, sep, autostart, sep, pair, sep, settings…, sep, quit]
            let menu = MenuBuilder::new(app)
                .items(&[
                    &status_item,
                    &PredefinedMenuItem::separator(app)?,
                    &autostart_item,
                    &PredefinedMenuItem::separator(app)?,
                    &pair_item,
                    &PredefinedMenuItem::separator(app)?,
                    &settings_item,
                    &PredefinedMenuItem::separator(app)?,
                    &MenuItemBuilder::new("Quit").id("quit").build(app)?,
                ])
                .build()?;

            let icon = tauri::include_image!("icons/32x32.png");

            // Clone handles for the event closure before ownership moves into TrayIconBuilder.
            let autostart_item_for_events = autostart_item.clone();

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
                    "settings" => {
                        settings_window::open(&app_for_events);
                    }
                    "toggle-autostart" => {
                        let app = app_for_events.clone();
                        let item = autostart_item_for_events.clone();
                        tauri::async_runtime::spawn(async move {
                            let autolaunch = app.autolaunch();

                            // Read current state from the OS plist (ground truth).
                            let currently_enabled = autolaunch.is_enabled().unwrap_or(false);

                            // Toggle the LaunchAgent registration. Only update the
                            // checkmark and persist the new state when the OS call
                            // succeeds (amendment B1). On failure, warn and leave the
                            // menu reflecting the actual registered state.
                            let result = if currently_enabled {
                                autolaunch.disable()
                            } else {
                                autolaunch.enable()
                            };

                            match result {
                                Ok(()) => {
                                    let new_state = !currently_enabled;
                                    // Update the checkmark on the main thread
                                    // (macOS panics on menu mutations off main thread).
                                    let _ = app.run_on_main_thread(move || {
                                        let _ = item.set_checked(new_state);
                                    });
                                    // Persist the new autostart state to the store.
                                    let mut settings = match AppSettings::load(&app) {
                                        Ok(s) => s,
                                        Err(e) => {
                                            warn!(
                                                "Failed to load app settings for autostart save: {e}"
                                            );
                                            return;
                                        }
                                    };
                                    settings.set_autostart_enabled(new_state);
                                    if let Err(e) = settings.save(&app) {
                                        warn!("Failed to save autostart state: {e}");
                                    }
                                }
                                Err(e) => {
                                    warn!("Failed to toggle autostart: {e}");
                                }
                            }
                        });
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
                autostart_item,
            };
            tray_status::start(handle.clone(), handles, status_rx);

            // Start the pairing-events consumer. Translates daemon-side
            // PairingUiEvents into the responder window + macOS notification.
            pair_events::start(handle.clone(), pairing_rx);

            Ok(())
        })
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            commands::start_pair_discovery,
            commands::request_pairing,
            commands::submit_pair_code,
            commands::cancel_pair_discovery,
            commands::reject_inbound_pair,
            commands::get_settings,
            commands::save_settings,
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
