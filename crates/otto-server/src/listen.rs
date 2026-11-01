//! The listeners `otto serve` can bind.
//!
//! A Unix socket is the default because file modes alone keep other local users
//! out. A TCP port is reachable by every local user and every page open in a
//! browser, so it is gated by a token. [`AllowedHosts::LoopbackOnly`] restricts
//! a plain TCP port to loopback addresses; [`AllowedHosts::Any`] accepts any IP
//! literal and is selected by the caller only once TLS and an environment token
//! are configured. TLS wraps the TCP listener in [`super::tls::TlsListener`].

use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use axum::extract::connect_info::Connected;
use axum::serve::IncomingStream;

/// Which listen addresses [`listen_tcp`] accepts.
///
/// `LoopbackOnly` is the default for a plain TCP port. `Any` is what the caller
/// selects once TLS and an environment token are configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowedHosts {
    LoopbackOnly,
    Any,
}

/// The `ConnectInfo` type the TCP and TLS listeners attach to every request;
/// the `http_request` log line reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerAddress(pub SocketAddr);

impl Connected<IncomingStream<'_, tokio::net::TcpListener>> for PeerAddress {
    fn connect_info(stream: IncomingStream<'_, tokio::net::TcpListener>) -> Self {
        Self(*stream.remote_addr())
    }
}

impl Connected<IncomingStream<'_, super::tls::TlsListener>> for PeerAddress {
    fn connect_info(stream: IncomingStream<'_, super::tls::TlsListener>) -> Self {
        Self(*stream.remote_addr())
    }
}

/// A bound listener, any flavour.
#[derive(Debug)]
pub enum Listener {
    Tcp(tokio::net::TcpListener),
    Tls(super::tls::TlsListener),
    Unix(tokio::net::UnixListener, SocketFile),
}

impl Listener {
    /// How the address prints in the `otto serve:` startup line.
    pub fn address(&self) -> String {
        match self {
            Self::Tcp(listener) => listener
                .local_addr()
                .map(|address| address.to_string())
                .unwrap_or_default(),
            Self::Tls(listener) => listener
                .local_addr()
                .map(|address| address.to_string())
                .unwrap_or_default(),
            Self::Unix(listener, _) => listener
                .local_addr()
                .ok()
                .and_then(|address| address.as_pathname().map(|path| path.display().to_string()))
                .unwrap_or_default(),
        }
    }
}

/// Removes the Unix socket file at `path` when dropped, but only if it still
/// names the same socket this listener bound (matched by device and inode):
/// by the time this drops, another `otto serve` may already have replaced a
/// stale path with its own socket, and that one must not be deleted. A
/// stat-then-unlink race window remains between the check and the removal.
#[derive(Debug)]
pub struct SocketFile {
    path: PathBuf,
    dev: u64,
    ino: u64,
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        let Ok(metadata) = std::fs::symlink_metadata(&self.path) else {
            return;
        };
        if !metadata.file_type().is_socket() {
            return;
        }
        if metadata.dev() != self.dev || metadata.ino() != self.ino {
            return;
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Creates a Unix domain socket listener at `path`.
///
/// The parent directory is created with mode 0700 if missing; if it already
/// exists it must be owned by the current user and not group- or
/// world-accessible. A leftover socket file at `path` is dialed to tell a
/// live server (rejected as "already running") from a stale one (removed and
/// replaced). Dropping the returned listener removes the socket file at
/// `path` unless the path now names a different file.
pub fn listen_unix(path: &str) -> Result<Listener, String> {
    let directory = Path::new(path).parent().unwrap_or(Path::new("."));
    ensure_socket_directory(directory)?;
    remove_stale_socket(path)?;

    let listener = tokio::net::UnixListener::bind(path)
        .map_err(|error| format!("listen on {path}: {error}"))?;
    if let Err(error) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        drop(listener);
        let _ = std::fs::remove_file(path);
        return Err(format!("chmod {path}: {error}"));
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            drop(listener);
            let _ = std::fs::remove_file(path);
            return Err(format!("stat socket {path}: {error}"));
        }
    };
    let socket_file = SocketFile {
        path: PathBuf::from(path),
        dev: metadata.dev(),
        ino: metadata.ino(),
    };
    Ok(Listener::Unix(listener, socket_file))
}

fn ensure_socket_directory(directory: &Path) -> Result<(), String> {
    let shown = directory.display();
    let metadata = match std::fs::metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            std::fs::create_dir_all(directory)
                .map_err(|error| format!("create socket directory {shown}: {error}"))?;
            return std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
                .map_err(|error| format!("create socket directory {shown}: {error}"));
        }
        Err(error) => return Err(format!("stat socket directory {shown}: {error}")),
    };
    if !metadata.is_dir() {
        return Err(format!("socket directory {shown} is not a directory"));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(format!(
            "socket directory {shown} must not be group- or world-accessible"
        ));
    }
    if metadata.uid() != current_uid() {
        return Err(format!(
            "socket directory {shown} must be owned by the current user"
        ));
    }
    Ok(())
}

fn current_uid() -> u32 {
    nix::unistd::geteuid().as_raw()
}

fn remove_stale_socket(path: &str) -> Result<(), String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("stat socket {path}: {error}")),
    };
    if !metadata.file_type().is_socket() {
        return Err(format!("socket path {path} exists and is not a socket"));
    }
    if std::os::unix::net::UnixStream::connect(path).is_ok() {
        return Err(format!("already running at {path}"));
    }
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("remove stale socket {path}: {error}")),
    }
}

/// Creates a TCP listener on `address` (`host:port`).
///
/// No DNS lookup happens in either mode: `localhost` maps to 127.0.0.1 and any
/// other host must be an IP literal. With [`AllowedHosts::LoopbackOnly`] the
/// literal must be a loopback address. Port 0 picks a free port; read the
/// result from `local_addr()`.
pub fn listen_tcp(address: &str, hosts: AllowedHosts) -> Result<tokio::net::TcpListener, String> {
    let Some((host, port)) = split_host_port(address) else {
        return Err(format!(
            "listen address {address:?}: missing port in address"
        ));
    };
    let Some(host) = resolve_host(host, hosts) else {
        return Err(match hosts {
            AllowedHosts::LoopbackOnly => format!(
                "listen address {address:?} is not a loopback address; a non-loopback address needs [server].tls_cert, [server].tls_key and OTTO_SERVER_TOKEN"
            ),
            AllowedHosts::Any => {
                format!("listen address {address:?}: host must be an IP literal or localhost")
            }
        });
    };
    let port: u16 = port
        .parse()
        .map_err(|_| format!("listen address {address:?}: unknown port"))?;
    let bound = std::net::TcpListener::bind(SocketAddr::new(host, port))
        .map_err(|error| format!("listen on {address}: {error}"))?;
    bound
        .set_nonblocking(true)
        .map_err(|error| format!("listen on {address}: {error}"))?;
    tokio::net::TcpListener::from_std(bound)
        .map_err(|error| format!("listen on {address}: {error}"))
}

/// Splits host and port for the forms an address may take here.
fn split_host_port(address: &str) -> Option<(&str, &str)> {
    if let Some(rest) = address.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        return Some((host, tail.strip_prefix(':')?));
    }
    let (host, port) = address.rsplit_once(':')?;
    if host.contains(':') {
        return None; // too many colons for an unbracketed address
    }
    Some((host, port))
}

/// Maps the literal `localhost` to 127.0.0.1 (no DNS lookup, so an
/// `/etc/hosts` entry cannot redirect the bind) and parses any other host as
/// an IP literal. `LoopbackOnly` additionally requires a loopback address;
/// `Any` accepts every literal. Returns `None` for a rejected host.
fn resolve_host(host: &str, hosts: AllowedHosts) -> Option<IpAddr> {
    if host == "localhost" {
        return Some(IpAddr::from([127, 0, 0, 1]));
    }
    let address: IpAddr = host.parse().ok()?;
    match hosts {
        AllowedHosts::LoopbackOnly => address.is_loopback().then_some(address),
        AllowedHosts::Any => Some(address),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_loopback_addresses_are_rejected_and_named() {
        for address in [
            "0.0.0.0:0",
            ":0",
            "192.168.1.1:0",
            "example.com:0",
            "127.0.0.1",
        ] {
            let error =
                listen_tcp(address, AllowedHosts::LoopbackOnly).expect_err("should be rejected");
            assert!(error.contains(address), "{error} does not name {address}");
        }
    }

    #[tokio::test]
    async fn port_zero_resolves_to_a_real_loopback_port() {
        let listener = listen_tcp("127.0.0.1:0", AllowedHosts::LoopbackOnly).expect("bind");
        let address = listener.local_addr().expect("socket address");
        assert!(address.port() != 0);
        assert!(address.ip().is_loopback());
    }

    #[tokio::test]
    async fn localhost_binds_to_the_ipv4_loopback_literal() {
        let listener = listen_tcp("localhost:0", AllowedHosts::LoopbackOnly).expect("bind");
        let address = listener.local_addr().expect("socket address");
        assert_eq!(address.ip(), IpAddr::from([127, 0, 0, 1]));
    }

    #[tokio::test]
    async fn any_host_mode_accepts_unspecified_and_private_ip_literals() {
        for address in ["0.0.0.0:0", "127.0.0.1:0"] {
            listen_tcp(address, AllowedHosts::Any)
                .unwrap_or_else(|error| panic!("{address}: {error}"));
        }
        // 192.168.1.1 is usually not a local address, so the bind may fail;
        // only a rejection by the host check is an error here.
        if let Err(error) = listen_tcp("192.168.1.1:0", AllowedHosts::Any) {
            assert!(!error.contains("is not a loopback address"), "{error}");
        }
    }

    #[test]
    fn any_host_mode_rejects_hostnames() {
        let error = listen_tcp("example.com:0", AllowedHosts::Any).expect_err("hostname");
        assert!(error.contains("IP literal"), "{error}");
    }

    #[test]
    fn loopback_only_error_names_the_three_requirements() {
        let error = listen_tcp("0.0.0.0:0", AllowedHosts::LoopbackOnly).expect_err("rejected");
        for required in ["[server].tls_cert", "[server].tls_key", "OTTO_SERVER_TOKEN"] {
            assert!(error.contains(required), "{error} lacks {required}");
        }
    }

    fn socket_path(directory: &tempfile::TempDir) -> String {
        directory
            .path()
            .join("sub")
            .join("otto.sock")
            .display()
            .to_string()
    }

    #[tokio::test]
    async fn the_parent_directory_is_created_private_and_the_socket_is_0600() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = socket_path(&directory);
        let listener = listen_unix(&path).expect("listen");

        let parent = std::fs::metadata(directory.path().join("sub")).expect("stat parent");
        assert!(parent.is_dir());
        assert_eq!(parent.permissions().mode() & 0o777, 0o700);
        let socket = std::fs::metadata(&path).expect("stat socket");
        assert_eq!(socket.permissions().mode() & 0o777, 0o600);
        drop(listener);
    }

    #[tokio::test]
    async fn a_second_listen_reports_already_running() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = socket_path(&directory);
        let _first = listen_unix(&path).expect("listen");
        let error = listen_unix(&path).expect_err("second listen");
        assert!(error.contains("already running"), "{error}");
        assert!(error.contains(&path), "{error}");
    }

    #[tokio::test]
    async fn a_stale_socket_is_replaced() {
        let directory = tempfile::tempdir().expect("tempdir");
        let parent = directory.path().join("sub");
        std::fs::create_dir(&parent).expect("mkdir");
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        let path = socket_path(&directory);
        // std's `UnixListener` does not unlink its path on drop, so this
        // leaves the same stale socket a killed `otto serve` leaves behind.
        let stale = std::os::unix::net::UnixListener::bind(&path).expect("bind stale");
        drop(stale);
        assert!(
            std::fs::symlink_metadata(&path)
                .expect("stale entry")
                .file_type()
                .is_socket()
        );
        let _second = listen_unix(&path).expect("stale socket should be replaced");
    }

    #[tokio::test]
    async fn dropping_the_listener_removes_the_socket_file() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = socket_path(&directory);
        let listener = listen_unix(&path).expect("listen");
        drop(listener);
        let error = std::fs::symlink_metadata(&path).expect_err("socket file should be removed");
        assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
    }

    #[tokio::test]
    async fn a_socket_replaced_before_drop_is_left_in_place() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = socket_path(&directory);
        let listener = listen_unix(&path).expect("listen");

        std::fs::remove_file(&path).expect("remove");
        let replacement = std::os::unix::net::UnixListener::bind(&path).expect("bind replacement");

        drop(listener);

        let metadata = std::fs::symlink_metadata(&path).expect("replacement should remain");
        assert!(metadata.file_type().is_socket());
        drop(replacement);
    }

    #[tokio::test]
    async fn an_existing_non_socket_file_is_preserved() {
        let directory = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(directory.path().join("sub")).expect("mkdir");
        let path = socket_path(&directory);
        std::fs::write(&path, b"keep me").expect("write");

        listen_unix(&path).expect_err("a regular file must be refused");
        assert_eq!(std::fs::read(&path).expect("read"), b"keep me");
    }

    #[tokio::test]
    async fn a_group_writable_parent_directory_is_refused() {
        let directory = tempfile::tempdir().expect("tempdir");
        let parent = directory.path().join("sub");
        std::fs::create_dir(&parent).expect("mkdir");
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o777)).expect("chmod");
        let error = listen_unix(&socket_path(&directory)).expect_err("must be refused");
        assert!(error.contains("group- or world-accessible"), "{error}");
    }
}
