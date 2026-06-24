//! Single-instance daemon flock now lives in `p2p_core::daemon_lock`.
//!
//! Re-exported here so `sync_daemon::daemon_lock::*` keeps resolving for the
//! daemon's own modules and the integration-test suite.
pub use p2p_core::daemon_lock::{DaemonLock, LockError};
