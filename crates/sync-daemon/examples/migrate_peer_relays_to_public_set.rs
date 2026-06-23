//! THROWAWAY one-shot upgrade tool: migrate a pre-public-relay `daemon.toml` to
//! the public-relay model.
//!
//! Earlier daemons persisted a `[[peer_relays]]` block — a per-peer list of
//! advertised relay URLs (public domains AND private LAN-IP relays mixed
//! together). The current daemon retired that field; the only durable
//! networking store is now `known_public_relays`, which must hold ONLY
//! off-LAN-reachable relays (public domains / globally-routable IPs), never a
//! private LAN-IP that is useless once the laptop leaves that LAN.
//!
//! This binary lifts the PUBLIC relay URLs out of the old `[[peer_relays]]`
//! entries and writes them (deduped) into `known_public_relays`, dropping the
//! private LAN-IP ones, and preserves the rest of the config untouched. It
//! parses the file as RAW TOML — independent of the daemon's config schema — so
//! it works regardless of how that schema has since drifted, and it reuses the
//! daemon's own [`relay_class::relay_is_offlan_reachable`] classifier so the
//! public/private split can never disagree with the daemon's.
//!
//! It is a CONVENIENCE, not a correctness gate: the new daemon tolerates an
//! empty `known_public_relays` (LAN-only until gossip re-learns the public
//! relay on the next home session). Running this just avoids that
//! "off-LAN-broken-until-next-home-session" window for the existing 3-machine
//! fleet. After every machine has run it, delete this example — it is a
//! single-upgrade tool, not permanent daemon code.
//!
//! Re-running is a no-op: a public relay already present in `known_public_relays`
//! is not re-added.
//!
//! ## Upgrade steps (run once per machine, while the daemon is stopped)
//!
//! 1. Stop the desktop app / daemon on the target machine.
//! 2. Back up the config:
//!    `cp <vault>/.sync/daemon.toml <vault>/.sync/daemon.toml.bak-<date>`.
//! 3. Run the migration:
//!    `cargo run -p sync-daemon --example migrate_peer_relays_to_public_set -- --config <vault>/.sync/daemon.toml`
//! 4. Verify: the printed summary lists the public relays it kept and the
//!    private ones it dropped; open `daemon.toml` and confirm
//!    `known_public_relays` holds the expected public URL(s).
//! 5. Pull + rebuild, then relaunch the daemon.

use std::path::PathBuf;
use std::process::ExitCode;

use sync_daemon::relay_class::relay_is_offlan_reachable;
use toml::Table;
use toml::Value;

fn main() -> ExitCode {
    let Some(config_path) = parse_args() else {
        eprintln!(
            "usage: cargo run -p sync-daemon --example migrate_peer_relays_to_public_set \
             -- --config <path-to-daemon.toml>"
        );
        return ExitCode::FAILURE;
    };

    match run(&config_path) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("error: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn parse_args() -> Option<PathBuf> {
    let mut args = std::env::args().skip(1);
    let mut config_path = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => config_path = args.next().map(PathBuf::from),
            _ => return None,
        }
    }
    config_path
}

fn run(config_path: &PathBuf) -> Result<(), String> {
    let contents = std::fs::read_to_string(config_path)
        .map_err(|e| format!("could not read {}: {e}", config_path.display()))?;
    let mut table: Table = contents
        .parse()
        .map_err(|e| format!("{} is not valid TOML: {e}", config_path.display()))?;

    let outcome = migrate_config(&mut table);

    let serialized =
        toml::to_string(&table).map_err(|e| format!("could not re-serialize config: {e}"))?;
    std::fs::write(config_path, serialized)
        .map_err(|e| format!("could not write {}: {e}", config_path.display()))?;

    report(&outcome);
    Ok(())
}

/// What the migration changed, for the operator-facing summary.
struct MigrationOutcome {
    /// Public relay URLs lifted from `[[peer_relays]]` and added to the set.
    added: Vec<String>,
    /// Private LAN-IP relay URLs dropped (not off-LAN-reachable).
    dropped: Vec<String>,
    /// Public relays already present in `known_public_relays` (left as-is).
    already_present: Vec<String>,
}

/// Lift the PUBLIC relay URLs out of the raw `[[peer_relays]]` block and merge
/// them (deduped, order-preserving) into `known_public_relays`, dropping the
/// private LAN-IP ones and preserving the rest of the config.
///
/// Operates on the parsed `toml::Table` so the transformation is filesystem-free
/// and directly testable. Everything outside `peer_relays` / `known_public_relays`
/// is left untouched.
fn migrate_config(table: &mut Table) -> MigrationOutcome {
    let mut known: Vec<String> = match table.get("known_public_relays") {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };

    let mut added = Vec::new();
    let mut dropped = Vec::new();
    let mut already_present = Vec::new();

    for relay_url in peer_relay_urls(table) {
        if !relay_is_offlan_reachable(&relay_url) {
            push_unique(&mut dropped, relay_url);
            continue;
        }
        if known.contains(&relay_url) {
            push_unique(&mut already_present, relay_url);
            continue;
        }
        known.push(relay_url.clone());
        push_unique(&mut added, relay_url);
    }

    table.insert(
        "known_public_relays".to_string(),
        Value::Array(known.into_iter().map(Value::String).collect()),
    );

    MigrationOutcome {
        added,
        dropped,
        already_present,
    }
}

/// The `relay_url` of every `[[peer_relays]]` entry, in file order. Entries
/// without a string `relay_url` are skipped — we only migrate what we can dial.
fn peer_relay_urls(table: &Table) -> Vec<String> {
    let Some(Value::Array(entries)) = table.get("peer_relays") else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| entry.get("relay_url").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

/// Push `value` only if it isn't already in `list` (order-preserving dedup).
fn push_unique(list: &mut Vec<String>, value: String) {
    if !list.contains(&value) {
        list.push(value);
    }
}

fn report(outcome: &MigrationOutcome) {
    if outcome.added.is_empty() && outcome.dropped.is_empty() {
        println!("No public relays to migrate (known_public_relays unchanged).");
    }
    for url in &outcome.added {
        println!("  + kept public relay  {url}");
    }
    for url in &outcome.already_present {
        println!("  = already present    {url}");
    }
    for url in &outcome.dropped {
        println!("  - dropped LAN-only    {url}");
    }
    println!("Done. Review daemon.toml, then pull + rebuild + relaunch.");
}

#[cfg(test)]
mod tests {
    use super::{Table, Value, migrate_config};

    /// An existing-mesh `daemon.toml` at upgrade time: a `[[peer_relays]]` block
    /// mixing a PUBLIC relay (`umbra.computer`) with a PRIVATE LAN-IP relay, plus
    /// the rest of the config (peer_id, mesh_name) the daemon owns.
    fn sample_old_config() -> &'static str {
        "peer_id = \"abc123\"\n\
         mesh_name = \"my-notes\"\n\n\
         [[peer_relays]]\n\
         endpoint_id = \"aaaa\"\n\
         relay_url = \"https://umbra.computer/\"\n\
         failure_count = 0\n\n\
         [[peer_relays]]\n\
         endpoint_id = \"bbbb\"\n\
         relay_url = \"http://192.168.68.50:3340/\"\n\
         failure_count = 2\n"
    }

    /// The migration keeps the PUBLIC relay, drops the PRIVATE LAN-IP one, and
    /// leaves the rest of the config (peer_id, mesh_name) intact — the exact
    /// upgrade an existing 3-machine fleet box goes through.
    #[test]
    fn keeps_public_relay_drops_private_lan_ip_and_preserves_config() {
        let mut table: Table = sample_old_config().parse().unwrap();

        let outcome = migrate_config(&mut table);

        // Only the public relay survives into known_public_relays.
        let known = table["known_public_relays"].as_array().unwrap();
        let known: Vec<&str> = known.iter().filter_map(Value::as_str).collect();
        assert_eq!(
            known,
            vec!["https://umbra.computer/"],
            "only the off-LAN-reachable relay belongs in the public set"
        );

        // The private LAN-IP relay was dropped, the public one was added.
        assert_eq!(outcome.added, vec!["https://umbra.computer/".to_string()]);
        assert_eq!(
            outcome.dropped,
            vec!["http://192.168.68.50:3340/".to_string()]
        );

        // The rest of the config is preserved untouched.
        assert_eq!(table["peer_id"].as_str(), Some("abc123"));
        assert_eq!(table["mesh_name"].as_str(), Some("my-notes"));
    }

    /// Re-running on an already-migrated config is a no-op: a public relay
    /// already in `known_public_relays` is reported as present, not re-added,
    /// so the set never accumulates duplicates.
    #[test]
    fn is_idempotent_when_public_relay_already_present() {
        let config = "peer_id = \"abc123\"\n\
                      known_public_relays = [\"https://umbra.computer/\"]\n\n\
                      [[peer_relays]]\n\
                      endpoint_id = \"aaaa\"\n\
                      relay_url = \"https://umbra.computer/\"\n";
        let mut table: Table = config.parse().unwrap();

        let outcome = migrate_config(&mut table);

        let known = table["known_public_relays"].as_array().unwrap();
        let known: Vec<&str> = known.iter().filter_map(Value::as_str).collect();
        assert_eq!(
            known,
            vec!["https://umbra.computer/"],
            "an already-present public relay must not be duplicated"
        );
        assert!(outcome.added.is_empty());
        assert_eq!(
            outcome.already_present,
            vec!["https://umbra.computer/".to_string()]
        );
    }
}
