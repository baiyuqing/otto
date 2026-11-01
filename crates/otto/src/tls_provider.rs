//! The process-wide rustls crypto provider.
//!
//! The workspace enables one rustls crypto provider, `ring`; the `tokio-rustls`
//! entry in the workspace `Cargo.toml` records why. reqwest is built with its
//! `rustls-no-provider` feature and panics in `ClientBuilder::build` when no
//! provider is installed as the process default, so every reqwest client in
//! this crate and in its tests is created through [`http_client_builder`] or
//! [`http_client`], and the TLS listener takes its provider from [`install`].

use std::sync::Arc;

use tokio_rustls::rustls::crypto::{CryptoProvider, ring};

/// Installs ring as the process default provider when none is installed yet,
/// and returns the process default.
pub fn install() -> Arc<CryptoProvider> {
    if let Some(installed) = CryptoProvider::get_default() {
        return Arc::clone(installed);
    }
    if let Err(installed) = ring::default_provider().install_default() {
        // Another thread installed its provider between the check and this
        // call; that one is the process default.
        return installed;
    }
    Arc::clone(CryptoProvider::get_default().expect("the provider installed above"))
}

/// `reqwest::Client::builder()` after [`install`].
pub fn http_client_builder() -> reqwest::ClientBuilder {
    install();
    reqwest::Client::builder()
}

/// A reqwest client with reqwest's default settings, after [`install`].
///
/// Panics when the client cannot be built, as `reqwest::Client::new` does.
pub fn http_client() -> reqwest::Client {
    http_client_builder()
        .build()
        .expect("build the reqwest client with default settings")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_sets_the_process_default_provider() {
        let installed = install();
        let default = CryptoProvider::get_default().expect("a process default provider");
        assert!(Arc::ptr_eq(&installed, default));
    }

    #[test]
    fn http_client_builder_builds_a_client_without_a_prior_install() {
        // Fails with reqwest's "No rustls crypto provider is configured" panic
        // when the builder does not install the provider first.
        let client = http_client_builder().build().expect("build the client");
        drop(client);
    }
}
