//! TLS termination for the TCP listener.
//!
//! rustls with the process-wide crypto provider (`crate::tls_provider`),
//! server side only, no client
//! certificates. Handshakes run in separate tasks, so one client that sends
//! nothing cannot block `accept`; each handshake is bounded by
//! [`HANDSHAKE_TIMEOUT`] and at most [`MAX_PENDING_HANDSHAKES`] run at once.

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use tokio_rustls::server::TlsStream;
use tokio_rustls::{TlsAcceptor, rustls};

use crate::log::Logger;

/// Time a client has to finish the TLS handshake after the TCP connection.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Handshakes in progress at once; further TCP connections are closed.
pub const MAX_PENDING_HANDSHAKES: usize = 64;

/// Reads the PEM files and builds the acceptor. The protocol list offers
/// `http/1.1` only, because the HTTP server here does not speak HTTP/2.
pub fn load_acceptor(cert_path: &str, key_path: &str) -> Result<TlsAcceptor, String> {
    let certificate_chain = CertificateDer::pem_file_iter(cert_path)
        .map_err(|error| format!("read TLS certificate {cert_path}: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read TLS certificate {cert_path}: {error}"))?;
    if certificate_chain.is_empty() {
        return Err(format!("TLS certificate {cert_path} holds no certificate"));
    }
    let private_key = PrivateKeyDer::from_pem_file(key_path)
        .map_err(|error| format!("read TLS key {key_path}: {error}"))?;
    let provider = crate::tls_provider::install();
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| format!("TLS certificate {cert_path} and key {key_path}: {error}"))?
        .with_no_client_auth()
        .with_single_cert(certificate_chain, private_key)
        .map_err(|error| format!("TLS certificate {cert_path} and key {key_path}: {error}"))?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// A TCP listener that completes a TLS handshake before handing a connection
/// to the HTTP server.
pub struct TlsListener {
    tcp: TcpListener,
    acceptor: TlsAcceptor,
    log: Arc<Logger>,
    pending_handshakes: JoinSet<Option<(TlsStream<TcpStream>, SocketAddr)>>,
}

impl std::fmt::Debug for TlsListener {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TlsListener")
            .field("local_addr", &self.tcp.local_addr().ok())
            .finish()
    }
}

impl TlsListener {
    pub fn new(tcp: TcpListener, acceptor: TlsAcceptor, log: Arc<Logger>) -> Self {
        Self {
            tcp,
            acceptor,
            log,
            pending_handshakes: JoinSet::new(),
        }
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.tcp.local_addr()
    }

    fn start_handshake(&mut self, stream: TcpStream, peer_address: SocketAddr) {
        if self.pending_handshakes.len() >= MAX_PENDING_HANDSHAKES {
            self.log.warn(
                "tls_handshake_dropped",
                &[("peer", peer_address.to_string())],
            );
            return;
        }
        let acceptor = self.acceptor.clone();
        let log = Arc::clone(&self.log);
        self.pending_handshakes.spawn(async move {
            let failure =
                match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                    Ok(Ok(tls_stream)) => return Some((tls_stream, peer_address)),
                    Ok(Err(error)) => error.to_string(),
                    Err(_) => format!("handshake not finished within {HANDSHAKE_TIMEOUT:?}"),
                };
            log.warn(
                "tls_handshake_failed",
                &[("peer", peer_address.to_string()), ("error", failure)],
            );
            None
        });
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            tokio::select! {
                Some(finished) = self.pending_handshakes.join_next(), if !self.pending_handshakes.is_empty() => {
                    // A panicked handshake task yields Err and is ignored.
                    if let Ok(Some(connection)) = finished {
                        return connection;
                    }
                }
                accepted = self.tcp.accept() => match accepted {
                    Ok((stream, peer_address)) => self.start_handshake(stream, peer_address),
                    Err(error) => {
                        // Same policy as axum's own TcpListener: retry a
                        // connection-level error at once, wait 1 s otherwise.
                        if !matches!(
                            error.kind(),
                            ErrorKind::ConnectionRefused | ErrorKind::ConnectionAborted | ErrorKind::ConnectionReset
                        ) {
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                    }
                },
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.tcp.local_addr()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::listen;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_rustls::TlsConnector;
    use tokio_rustls::rustls::pki_types::ServerName;
    use tokio_util::sync::CancellationToken;

    const REQUEST: &[u8] = b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";

    struct LogSink(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct Identity {
        cert_der: CertificateDer<'static>,
        cert_pem: String,
        key_pem: String,
    }

    fn generate_identity() -> Identity {
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("generate");
        Identity {
            cert_der: cert.der().clone(),
            cert_pem: cert.pem(),
            key_pem: signing_key.serialize_pem(),
        }
    }

    fn write_identity(
        directory: &std::path::Path,
        identity: &Identity,
        name: &str,
    ) -> (String, String) {
        let cert_path = directory.join(format!("{name}.crt"));
        let key_path = directory.join(format!("{name}.key"));
        std::fs::write(&cert_path, &identity.cert_pem).expect("write cert");
        std::fs::write(&key_path, &identity.key_pem).expect("write key");
        (
            cert_path.display().to_string(),
            key_path.display().to_string(),
        )
    }

    fn connector(identity: &Identity) -> TlsConnector {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(identity.cert_der.clone()).expect("add root");
        let provider = crate::tls_provider::install();
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        TlsConnector::from(Arc::new(config))
    }

    struct Running {
        address: SocketAddr,
        identity: Identity,
        cancel: CancellationToken,
        task: tokio::task::JoinHandle<Result<(), String>>,
        log: Arc<Mutex<Vec<u8>>>,
        _directory: tempfile::TempDir,
    }

    async fn start_server() -> Running {
        let directory = tempfile::tempdir().expect("tempdir");
        let identity = generate_identity();
        let (cert_path, key_path) = write_identity(directory.path(), &identity, "server");
        let acceptor = load_acceptor(&cert_path, &key_path).expect("acceptor");
        let tcp =
            listen::listen_tcp("127.0.0.1:0", listen::AllowedHosts::LoopbackOnly).expect("bind");
        let log = Arc::new(Mutex::new(Vec::new()));
        let logger = Arc::new(Logger::new(Box::new(LogSink(Arc::clone(&log)))));
        let listener = TlsListener::new(tcp, acceptor, logger);
        let address = listener.local_addr().expect("local addr");
        let router = axum::Router::new().route("/healthz", axum::routing::get(|| async { "ok" }));
        let cancel = CancellationToken::new();
        let task = tokio::spawn(super::super::serve(
            listen::Listener::Tls(listener),
            router,
            cancel.clone(),
        ));
        Running {
            address,
            identity,
            cancel,
            task,
            log,
            _directory: directory,
        }
    }

    async fn tls_get(running: &Running) -> String {
        let stream = TcpStream::connect(running.address).await.expect("connect");
        let name = ServerName::try_from("localhost").expect("server name");
        let mut tls = connector(&running.identity)
            .connect(name, stream)
            .await
            .expect("handshake");
        tls.write_all(REQUEST).await.expect("write");
        let mut response = Vec::new();
        let _ = tls.read_to_end(&mut response).await;
        String::from_utf8_lossy(&response).into_owned()
    }

    async fn stop(running: Running) {
        running.cancel.cancel();
        running.task.await.expect("join").expect("serve");
    }

    #[test]
    fn load_acceptor_reads_pem_files() {
        let directory = tempfile::tempdir().expect("tempdir");
        let (cert_path, key_path) = write_identity(directory.path(), &generate_identity(), "pair");
        load_acceptor(&cert_path, &key_path).expect("acceptor");
    }

    #[test]
    fn load_acceptor_errors_name_the_missing_file() {
        let directory = tempfile::tempdir().expect("tempdir");
        let (cert_path, key_path) = write_identity(directory.path(), &generate_identity(), "first");
        let (_other_cert_path, other_key_path) =
            write_identity(directory.path(), &generate_identity(), "second");
        let missing = directory.path().join("missing.pem").display().to_string();

        let error = load_acceptor(&missing, &key_path)
            .err()
            .expect("missing cert");
        assert!(error.contains(&missing), "{error}");
        let error = load_acceptor(&cert_path, &missing)
            .err()
            .expect("missing key");
        assert!(error.contains(&missing), "{error}");
        let error = load_acceptor(&cert_path, &other_key_path)
            .err()
            .expect("mismatched pair");
        assert!(
            error.contains(&cert_path) && error.contains(&other_key_path),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_tls_client_completes_a_request_through_serve() {
        let running = start_server().await;
        let response = tls_get(&running).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        stop(running).await;
    }

    #[tokio::test]
    async fn a_stalled_plain_tcp_client_does_not_block_a_tls_client() {
        let running = start_server().await;
        let stalled = TcpStream::connect(running.address)
            .await
            .expect("raw connect");
        let response = tokio::time::timeout(Duration::from_secs(3), tls_get(&running))
            .await
            .expect("TLS client finished while a raw client was stalled");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        drop(stalled);
        stop(running).await;
    }

    #[tokio::test]
    async fn a_client_sending_garbage_is_logged_and_the_listener_keeps_serving() {
        let running = start_server().await;
        let mut raw = TcpStream::connect(running.address)
            .await
            .expect("raw connect");
        raw.write_all(b"not a tls hello\n")
            .await
            .expect("write garbage");
        let response = tls_get(&running).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        // The failed handshake runs in its own task, so the log line may land
        // after the TLS client's response; wait up to 2 s for it.
        let mut logged = String::new();
        for _ in 0..20 {
            logged =
                String::from_utf8_lossy(&running.log.lock().unwrap_or_else(|p| p.into_inner()))
                    .into_owned();
            if logged.contains("tls_handshake_failed") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(logged.contains("tls_handshake_failed"), "{logged}");
        stop(running).await;
    }
}
