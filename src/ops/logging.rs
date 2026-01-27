use tracing_subscriber::EnvFilter;

#[allow(dead_code)] // prefer init_with_default(); kept for compatibility with early scaffolding
pub fn init() {
    init_with_default("info");
}

pub fn init_with_default(default_level: &str) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_timer(tracing_subscriber::fmt::time::SystemTime)
        .try_init();
}
