use tracing_subscriber::EnvFilter;

/// Initialize tracing with sensible defaults.
///
/// Respects `RUST_LOG` env var. Falls back to `info` level (or `debug` when verbose).
/// Module-specific filtering uses the provided `module_name` for targeted debug output.
pub fn init_tracing(verbose: bool, module_name: &str) {
    let default_filter = if verbose {
        format!("debug,{}=debug", module_name)
    } else {
        format!("info,{}=info", module_name)
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
