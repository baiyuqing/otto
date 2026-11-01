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
    // `install_default` returns `Err` with the rejected provider when another
    // thread installed one between the check above and this call; the process
    // default is then that other provider, so it is read back in both cases.
    let _ = ring::default_provider().install_default();
    Arc::clone(
        CryptoProvider::get_default().expect("a provider is installed above or by another thread"),
    )
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
    use std::sync::Barrier;
    use std::thread;

    use super::*;

    #[test]
    fn install_sets_the_process_default_provider() {
        let installed = install();
        let default = CryptoProvider::get_default().expect("a process default provider");
        assert!(Arc::ptr_eq(&installed, default));
    }

    /// Threads that call `install()` at the same time all get the one provider
    /// that won the installation. Without the fix, the losing threads got the
    /// provider that `install_default` rejected, so `Arc::ptr_eq` fails here
    /// when this test is the first in the process to install a provider.
    #[test]
    fn concurrent_installs_all_return_the_process_default() {
        const THREADS: usize = 8;
        let barrier = Arc::new(Barrier::new(THREADS));
        let workers: Vec<_> = (0..THREADS)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    install()
                })
            })
            .collect();
        let installed: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().expect("install() does not panic"))
            .collect();
        // Read the default only after every worker returned: before that, no
        // provider may exist yet.
        let default = CryptoProvider::get_default().expect("a process default provider");
        for provider in &installed {
            assert!(Arc::ptr_eq(provider, default));
        }
    }

    #[test]
    fn http_client_builder_builds_a_client_without_a_prior_install() {
        // Fails with reqwest's "No rustls crypto provider is configured" panic
        // when the builder does not install the provider first.
        let client = http_client_builder().build().expect("build the client");
        drop(client);
    }
}
