# Obsidian Memory — Desktop App

A native macOS menu-bar app that runs the Obsidian Memory sync daemon and
exposes its pairing flow through a tray UI. The app embeds `sync-daemon` as
a Tokio task — there is no separate daemon process to start, no CLI to keep
running.

This is a Tauri v2 app. The Rust crate is `crates/desktop`; the frontend lives
under `crates/desktop/frontend` (Vite + plain HTML; the pairing windows are
static files served from `public/windows/`, no React).

## Status

Phase 1.5 of the [Memory Desktop App](../../) effort. Ships in v0.5.x as the
release-blocker for restoring internet-traversing sync after the plugin's
embedded relay was removed in commit `4eec6c1`.

**What works:**

- macOS menu bar tray with live status (Idle / Connected · N peers).
- "Pair with nearby device…" menu item opens an initiator window that scans
  the LAN via mDNS and submits a 6-digit code to the responder.
- Inbound pairing requests pop a responder window showing the code + a
  5-minute countdown. Reject button + macOS notification included.
- Embedded iroh relay listens on `0.0.0.0:3340` and advertises the machine's
  detected LAN IP so peers on the same network can reach it.
- Quit handler cleanly shuts the daemon down and clears `relay_url` in
  `daemon.toml`.

**Out of scope for v0.5.x:** cross-network pairing (invite codes / URLs are
Phase 6), auto-port-forwarding, the React popup UI on left-click, code
signing, auto-update.

## Running

The vault path is read from `OBSIDIAN_MEMORY_VAULT` (no `--vault` flag).

> **Note:** `npm run tauri dev` / `npm run tauri build` do **not** work with
> this crate's layout — the frontend is a subfolder of the tauri crate, so
> tauri-cli resolves the wrong cwd, and there's no `tsconfig.json` for the
> `tsc` build step. Run the binary directly instead; `build.rs` embeds the
> built frontend, so no dev server is needed:

```bash
# 1. Build the frontend once (emits frontend/dist incl. the pairing windows)
cd crates/desktop/frontend && npm install && npm run build

# 2. Run the app (compiles the crate, starts the daemon + tray)
cd crates/desktop && OBSIDIAN_MEMORY_VAULT=~/notes cargo run
```

On first run the tray icon appears in the menu bar. The app is dock-less by
design — the `Accessory` activation policy is set in `main.rs`. The health
endpoint defaults to `127.0.0.1:8081`; change the `health_listen` port in
`main.rs` if it's taken.

A packaged release `.app` (`tauri build`) isn't wired up for this layout yet —
tracked as a follow-up.

## Architecture

The crate is laid out so each module owns one concern:

- `daemon_task.rs` — spawns `sync_daemon::daemon::run_with_shutdown_controlled`
  as a Tauri async task, returns a `DaemonHandle` carrying the cancellation
  token, `DaemonControl`, and watchdog `done_rx`. The watchdog calls
  `app.exit(1)` if the daemon errors or panics.
- `shutdown.rs` — `ShutdownController` is taken once by the Quit handler and
  awaits the watchdog for up to 5s before force-exiting.
- `tray_status.rs` — driver task subscribing to `DaemonControl.status_rx`,
  updates the cached `MenuItem` handles via `MenuItem::set_text` (no full
  menu rebuild — avoids the macOS flicker).
- `pair_window.rs` — `open_initiator(app)` and `open_responder(app, ...)`.
  Both wrap their `WebviewWindowBuilder` calls in `app.run_on_main_thread`;
  off-main-thread WebView creation panics on macOS. Close handlers fire
  `DaemonCommand::CancelInitiate` / `RejectInbound` so the daemon drops its
  in-flight session when the user closes via X / Cmd+W.
- `pair_events.rs` — consumer task subscribing to
  `DaemonControl.pairing_rx`. Translates each `PairingUiEvent` into the right
  UI action: `InboundRequest` → notification + responder window;
  `InboundCompleted` / `InboundFailed` → status event to the responder window
  + a delayed-close backstop.
- `notification.rs` — thin wrapper around `tauri-plugin-notification`. The
  notification is best-effort — failure does not block the pairing flow.
- `commands.rs` — Tauri `invoke` handlers: `start_pair_discovery`,
  `submit_pair_code`, `cancel_pair_discovery`, `reject_inbound_pair`.
- `frontend/public/windows/` — vanilla static HTML/JS. Vite copies these
  files into `dist/` at build verbatim. The pairing UI does not go through
  the React build pipeline.

## Cross-network pairing

v0.5.x scopes pairing to the LAN. Discovery uses mDNS; the embedded relay
binds to `0.0.0.0:3340` so it accepts peers from any interface. If a user
wants to expose the relay across the internet, that's their responsibility:

- Port-forward `:3340` on the router and tell peers your public address.
- Or run a TLS-terminating reverse proxy (Caddy, nginx) in front of the
  relay; daemon.toml's `relay_url` accepts an arbitrary string.
- Or wait for Phase 6's invite-code flow, which will bundle the relay URL
  into a shareable invite payload.

The app itself doesn't try to template Caddy configs, hole-punch through
NATs, or expose UPnP. Those decisions are best made by the user with
visibility into their network.

## Known follow-ups

These are non-blocking but worth tracking:

- `crates/sync-daemon/src/daemon.rs` is ~1600 lines and growing. Worth
  splitting into something like `daemon/mod.rs` + `daemon/initiator.rs` +
  `daemon/responder.rs` before Phase 6 adds more handlers.
- `run_initiator_pairing` (daemon.rs) and `pair.rs::pair_inner` (CLI) share
  the post-pair allowlist-write logic. Extracting into a `pair_shared.rs`
  helper would prevent drift. Reviewer flagged in Wave A.
- `SyncNode::subscribe_discovery()` is called twice in production (once by
  the daemon's run_loop, once per `DaemonCommand::StartDiscovery`). Verified
  in passing that iroh's `MdnsAddressLookup` supports multiple subscribers;
  if a regression ever appears, fall back to a fanout `broadcast` in
  `SyncNode` (~50 lines).
- macOS notification permission flow ([Phase 1.5 plan][plan] S7) requires
  manual smoke on a fresh bundle. The notification fallback path (denied
  permission → responder window still opens) is wired but not yet validated
  end-to-end.

## See also

- [`specs/sync/desktop-pairing.feature`](../../specs/sync/desktop-pairing.feature)
  — BDD spec for the pairing UX.
- [Memory Desktop App project note] — long-form design rationale (private).
- Phase 1.5 plan — full architecture + verification checklist (private).

[plan]: ../../../.claude/plans/desktop-phase-1-5-tray-status-lan-pairing.md
[Memory Desktop App project note]: obsidian://memory-desktop-app
