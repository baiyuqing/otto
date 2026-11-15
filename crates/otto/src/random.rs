//! Random bytes from the operating system, for identifiers and nonces.

use std::fs::File;
use std::io::Read;

/// `N` bytes from `/dev/urandom`, hex encoded. Same source as `randomID` in
/// `runtime::builder` and the workspace write tool.
pub(crate) fn random_hex<const N: usize>() -> std::io::Result<String> {
    Ok(crate::hex::to_hex(&random_bytes::<N>()?))
}

/// `N` raw bytes from `/dev/urandom`.
pub(crate) fn random_bytes<const N: usize>() -> std::io::Result<[u8; N]> {
    let mut bytes = [0u8; N];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}
