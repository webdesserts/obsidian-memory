# Desktop Sync Smoke Test (v0.5.x)

> Durable in-repo runbook to bring up the new v0.5.x architecture (Tauri desktop app + embedded sync-daemon) on **umbra** + a satellite Mac and validate P2P sync on LAN. Kept in the repo (not the notes vault) so it survives the vault wipe in Phase B. Hard-break migration — Michael is the only user, so we wipe satellites and reseed rather than preserve old pairings.

**Execution roles:** Laptop-side steps are run by the Claude instance on the personal laptop. umbra-side steps are run by Michael or the umbra Claude session (umbra = this dev machine). The 6-digit pairing code is relayed by Michael between the two.

## Goal

Get umbra and the personal laptop both running the v0.5.x Tauri app and **syncing the same vault**, so we can start dogfooding the new architecture. Validates build-from-source, the vault-seeding recipe, tray pairing, and the CRDT/QUIC/gossip sync engine on real LAN.

## Why "seed from umbra" instead of "pair two fresh vaults"

**Known gap (deferred TODO, also tracked in the project notes):** tray pairing reconciles the *allowlist* but **not the gossip topic**. A vault's gossip topic is derived from its random per-vault VaultId in `.sync/metadata.toml` (`crates/sync-core/src/network/node.rs:251`). The pairing handler (`crates/sync-daemon/src/daemon.rs::run_initiator_pairing`, ~lines 962–1017) receives the mesh's topic as `PairingResult.vault_topic` but drops it; the daemon only joins gossip on its *own* VaultId at startup (`daemon.rs:1292,1507`) and never re-joins. So two independently-created vaults pair "successfully" but land on different topics → they never sync.

We sidestep this by **seeding the satellite from umbra's vault** — the VaultId rides along in the copied `.sync/metadata.toml`, so both share a topic. Pairing then only has to do its working half (the allowlist). The real fix (new device adopts the mesh's `vault_topic` on pair) is deferred until after dogfooding starts.

## Topology

- **umbra** — this machine (home server + dev host + main relay). Has a GUI → runs the **Tauri desktop app**, replacing the old Docker `sync-daemon`. Source of truth. LAN IP `192.168.68.59`.
- **Personal laptop** — Mac with GUI → runs the **Tauri desktop app**. Satellite, seeded from umbra.
- Both on the home LAN. On LAN, peers connect **directly via QUIC**; umbra's relay is the cross-network fallback and is **not** exercised by this test.

## Prerequisites

0. **v0.5.x code on the personal laptop.** As of writing, v0.5.x with the Phase 1.5 desktop app is **local-only on umbra — not pushed to origin.** The laptop can't build the app until it's pushed or otherwise transferred. On the laptop, verify with `git ls-remote origin v0.5.x`, then `git fetch && git checkout v0.5.x && git pull`. The Phase 1.5 tip is commit `8f9bfa4` (`docs(desktop): remove stale event-based init comment…`).
1. Toolchain on each Mac: Rust (stable), Node + npm, and `cargo install tauri-cli --version "^2"`. No `wasm-pack` needed — the Obsidian plugin is skipped for the core sync test (the desktop app's embedded daemon watches vault files directly).

## `.sync/` file taxonomy (the seeding-critical part)

When seeding the satellite, **per-DEVICE** files must NOT carry over — delete them on the satellite so they regenerate fresh, otherwise you clone umbra's network identity and break the mesh:

- `.sync/daemon.key` — ed25519 identity → PeerId
- `.sync/daemon.toml` — advertises PeerId + relay_url
- `.sync/daemon.lock` — transient flock
- `.sync/known_peers.json` — peer discovery cache

**Per-VAULT** files must be kept identical — this is how both machines share a topic and a consistent CRDT history:

- `.sync/metadata.toml` — VaultId + schema version ← the critical shared file
- `.sync/registry.loro` + any document CRDT state under `.sync/` — copied so the CRDT history is consistent. (Rebuilding independently from `.md` files risks divergent op histories → duplicated content. Copy the `.loro` state; don't let the satellite rebuild it.)

## Phase A — Prove it on a SCRATCH vault (junk data, zero risk)

Do this first. If the recipe is wrong, we find out on throwaway data, not real notes.

**umbra side:**
1. Stop the old Docker `sync-daemon` (service under `docker/` at `/opt/docker/`). It binds the health port `8081` (and possibly relay `3340`), which the Tauri app also needs — they can't coexist. (The obsidian-memory MCP server is separate and keeps running.)
2. Create a scratch vault: `mkdir ~/sync-test` with 2–3 junk `.md` files.
3. Launch the Tauri app on it, from the repo:
   - `cd crates/desktop/frontend` (first run here: `npm install`)
   - nushell: `with-env {OBSIDIAN_MEMORY_VAULT: "/Users/nir/sync-test"} { npm run tauri dev }` — use an **absolute** path.
4. Confirm: tray icon appears (no dock icon), `curl http://127.0.0.1:8081/health` → 200, `~/sync-test/.sync/metadata.toml` exists. Then **Quit** the app (tray → Quit) so the vault is at rest for copying.

**Seed the laptop (handoff):**
5. Copy umbra's whole `~/sync-test` dir to the laptop over LAN (scp/rsync/AirDrop), e.g. to `/Users/<you>/sync-test`.
6. On the laptop, delete the 4 per-device files (any that exist) so the satellite gets a fresh identity:
   `rm sync-test/.sync/daemon.key sync-test/.sync/daemon.toml sync-test/.sync/daemon.lock sync-test/.sync/known_peers.json`

**laptop side:**
7. Launch the Tauri app on the seeded vault — same command, `OBSIDIAN_MEMORY_VAULT=/Users/<you>/sync-test`.
8. Confirm tray + health, and that the junk notes copied over are present.

**Pair + verify sync (both apps running):**
9. On one machine: tray → "Pair with nearby device…". It scans mDNS and lists the other machine's mesh. Select it.
10. The other machine pops a responder window + macOS notification with a **6-digit code**. Read it, enter it on the initiator. (First inbound pair may trigger a macOS notification-permission prompt — allow it. To re-test the prompt later: `tccutil reset Notifications com.webdesserts.obsidian-memory`.)
11. Confirm pairing success — each side's `.sync/allowlist.json` now lists the other's PeerId.
12. **The sync test:** edit a `.md` on umbra → it should appear on the laptop within a few seconds, and vice versa. Add a new file, delete a file — confirm each direction propagates.
13. ✅ Bidirectional sync working → recipe + engine proven, proceed to Phase B. ❌ Pairs but nothing syncs → the VaultId didn't carry over; check that both `.sync/metadata.toml` are identical before going further.

## Phase B — Hard-break migrate the real `~/notes`

Only after Phase A passes.

1. **Back up first.** On umbra and the laptop, make a full timestamped copy of the real `~/notes` off to the side. This is the safety net for the known corruption risk (this project has a vault-corruption history).
2. **umbra:** ensure the old Docker daemon is stopped (done in Phase A step 1).
3. **umbra:** launch the Tauri app on the real vault — `OBSIDIAN_MEMORY_VAULT=/Users/nir/notes npm run tauri dev`. First launch runs the v0→v1 migration (generates VaultId, builds CRDT state from your notes). umbra is now the source of truth. (The obsidian-memory MCP server also writes this vault — fine, the daemon watches files and coexists.) Let it settle, then Quit for the copy.
4. **laptop:** wipe the old `~/notes` (it's backed up) and seed it from umbra's migrated `~/notes` using the Phase A recipe (copy the whole dir, delete the 4 per-device files).
5. **laptop:** launch the Tauri app on `~/notes`, pair with umbra (steps 9–11), confirm the real notes converge.
6. From here, both run the app continuously and dogfood.

## Out of scope / deferred

- **Pairing topic-reconciliation fix** — the known gap described above. Until fixed, every new device must be **seeded** from an existing mesh member; you can't just pair a fresh vault.
- **Relay path** — LAN peers go direct; umbra's relay isn't exercised. Needs an off-LAN device to test.
- **Mobile** — no working mobile sync in v0.5.x (WASM can't bind UDP in the mobile webview). Skip the phone.
- **Obsidian plugin** — not needed for sync. Install later for the in-app dashboard: `cd plugin && npm install && npm run build:wasm && npm run build`, then copy `plugin/dist/obsidian-p2p-sync/*` → `<vault>/.obsidian/plugins/obsidian-p2p-sync/`.
- **Persistent umbra service** — `npm run tauri dev` is a foreground dev process. Turning umbra's app into a launch-at-boot service (built `.app` + launchd) is a follow-up once the flow is proven.

## Fallback: headless pairing (only if umbra's GUI tray is unavailable)

umbra has a GUI, so use the tray. If ever needed headless: with the daemon running, on the other device run `memory sync pair --vault <path>` (initiator scans mDNS, prompts for the code on stdin); the responder's 6-digit code prints to the daemon's stderr/logs. Same wire protocol. Note this CLI path also only does the allowlist half — it still relies on the shared-VaultId seed.
