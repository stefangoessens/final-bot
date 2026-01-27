pub fn install_rustls_provider() {
    // Idempotent: returns Err if a provider is already installed.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}
