//! Daemon config + relay-hint persistence now lives in `p2p_core::config`.
//!
//! Re-exported here so `sync_daemon::persistence::*` keeps resolving for the
//! daemon's own modules and the integration-test suite. The on-disk format
//! (`.sync/daemon.toml`) is owned by p2p-core; this is a compatibility shim.
pub use p2p_core::{DaemonConfig, PeerRelay, persist_config_change};
