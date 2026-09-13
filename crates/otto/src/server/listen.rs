//! The two listeners `otto serve` can bind.
//!
//! Port of `internal/server/listen.go` and `internal/server/listen_tcp.go`.
//!
//! A Unix socket is the default because file modes alone keep other local
//! users out. A TCP port is reachable by every local user and every page open
//! in a browser, so it is restricted to loopback and gated by the per-process
//! token in [`Options::token`](super::Options).

use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::Path;

/// A bound listener, either flavour.
#[derive(Debug)]
pub enum Listener {
    Tcp(tokio::net::TcpListener),
    Unix(tokio::net::UnixListener),
}

impl Listener {
    /// How the address prints in the `otto serve:` startup line. Go uses
    /// `net.Listener.Addr().String()`.
    pub fn address(&self) -> String {
        match self {
            Self::Tcp(listener) => listener
                .local_addr()
                .map(|address| address.to_string())
                .unwrap_or_default(),
            Self::Unix(listener) => listener
                .local_addr()
                .ok()
                .and_then(|address| address.as_pathname().map(|path| path.display().to_string()))
                .unwrap_or_default(),
        }
    }
}

/// Creates a Unix domain socket listener at `path`.
///
/// The parent directory is created with mode 0700 if missing; if it already
/// exists it must be owned by the current user and not group- or
/// world-accessible. A leftover socket file at `path` is dialed to tell a
/// live server (rejected as "already running") from a stale one (removed and
/// replaced).
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
    Ok(Listener::Unix(listener))
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
/// Only loopback hosts are accepted: the token is the only thing separating
/// the API from other local users, and it is not a substitute for TLS on a
/// shared network. Port 0 picks a free port; read the result from
/// [`Listener::address`].
pub fn listen_tcp(address: &str) -> Result<Listener, String> {
    let Some((host, port)) = split_host_port(address) else {
        return Err(format!(
            "listen address {address:?}: missing port in address"
        ));
    };
    let Some(host) = loopback_host(host) else {
        return Err(format!(
            "listen address {address:?} is not a loopback address; use 127.0.0.1, ::1, or localhost"
        ));
    };
    let port: u16 = port
        .parse()
        .map_err(|_| format!("listen address {address:?}: unknown port"))?;
    let bound = std::net::TcpListener::bind(SocketAddr::new(host, port))
        .map_err(|error| format!("listen on {address}: {error}"))?;
    bound
        .set_nonblocking(true)
        .map_err(|error| format!("listen on {address}: {error}"))?;
    let listener = tokio::net::TcpListener::from_std(bound)
        .map_err(|error| format!("listen on {address}: {error}"))?;
    Ok(Listener::Tcp(listener))
}

/// Port of `net.SplitHostPort` for the forms an address may take here.
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
/// `/etc/hosts` entry cannot redirect the bind) and accepts any loopback IP
/// literal. Relax this function, not its callers, when a non-loopback bind
/// behind TLS is added.
fn loopback_host(host: &str) -> Option<IpAddr> {
    if host == "localhost" {
        return Some(IpAddr::from([127, 0, 0, 1]));
    }
    let address: IpAddr = host.parse().ok()?;
    address.is_loopback().then_some(address)
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
            let error = listen_tcp(address).expect_err("should be rejected");
            assert!(error.contains(address), "{error} does not name {address}");
        }
    }

    #[tokio::test]
    async fn port_zero_resolves_to_a_real_loopback_port() {
        let listener = listen_tcp("127.0.0.1:0").expect("bind");
        let address: SocketAddr = listener.address().parse().expect("socket address");
        assert!(address.port() != 0);
        assert!(address.ip().is_loopback());
    }

    #[tokio::test]
    async fn localhost_binds_to_the_ipv4_loopback_literal() {
        let listener = listen_tcp("localhost:0").expect("bind");
        let address: SocketAddr = listener.address().parse().expect("socket address");
        assert_eq!(address.ip(), IpAddr::from([127, 0, 0, 1]));
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
        let path = socket_path(&directory);
        let first = listen_unix(&path).expect("listen");
        // Nothing unlinks the path, so dropping the listener leaves exactly
        // the stale socket a killed process leaves behind.
        drop(first);
        assert!(
            std::fs::symlink_metadata(&path)
                .expect("stale entry")
                .file_type()
                .is_socket()
        );
        let _second = listen_unix(&path).expect("stale socket should be replaced");
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
