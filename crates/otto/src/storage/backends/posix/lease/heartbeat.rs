//! Lease metadata and heartbeat record encoding.

use super::*;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct LeaseMeta {
    pub(super) lease_seconds: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct HeartbeatRecord {
    pub(super) epoch: u64,
    pub(super) host: String,
    pub(super) pid: u32,
    pub(super) released: bool,
    pub(super) seq: u64,
}

pub(super) fn write_heartbeat_record(
    dir: &fsops::Dir,
    record: &HeartbeatRecord,
) -> std::io::Result<()> {
    let json = serde_json::to_string(record).map_err(std::io::Error::other)?;
    if json.len() > HEARTBEAT_SIZE - 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "heartbeat record too large for its fixed-size slot",
        ));
    }
    let mut bytes = vec![b' '; HEARTBEAT_SIZE];
    bytes[..json.len()].copy_from_slice(json.as_bytes());
    bytes[HEARTBEAT_SIZE - 1] = b'\n';
    let file = fsops::create_at_no_follow(dir, "heartbeat", libc::O_WRONLY, 0o600)?;
    pwrite_all(&file, &bytes, 0)?;
    file.sync_all()?;
    Ok(())
}

pub(super) fn read_heartbeat_bytes(dir: &fsops::Dir) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut file = fsops::open_at_no_follow(dir, "heartbeat", libc::O_RDONLY)?;
    let mut buf = Vec::with_capacity(HEARTBEAT_SIZE);
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

pub(super) fn parse_heartbeat(bytes: &[u8]) -> std::io::Result<HeartbeatRecord> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    serde_json::from_str(text.trim_end())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Host and pid to report for a holder whose heartbeat bytes did not parse:
/// some other version's record, or a write caught mid-write. Acquisition
/// never fails on this; it reports an unidentified holder instead.
pub(super) fn parse_heartbeat_lenient(bytes: &[u8]) -> (String, u32) {
    match parse_heartbeat(bytes) {
        Ok(record) => (record.host, record.pid),
        Err(_) => ("unknown".to_string(), 0),
    }
}

/// Like [`read_heartbeat_bytes`], but a missing heartbeat file reads as
/// empty bytes rather than an error, for callers polling for one to appear.
pub(super) fn read_heartbeat_bytes_lenient(dir: &fsops::Dir) -> std::io::Result<Vec<u8>> {
    match read_heartbeat_bytes(dir) {
        Ok(bytes) => Ok(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// The current heartbeat, parsed. `None` covers both a missing heartbeat
/// file and one whose bytes do not parse: [`acquire_with`] treats an
/// unparseable heartbeat the same as an absent one, so a holder's initial
/// record being some other version, or caught mid-write, never turns
/// acquisition into a hard error -- it waits out the liveness window
/// instead.
pub(super) fn read_heartbeat(dir: &fsops::Dir) -> std::io::Result<Option<HeartbeatRecord>> {
    match read_heartbeat_bytes(dir) {
        Ok(bytes) => Ok(parse_heartbeat(&bytes).ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

pub(super) fn pwrite_all(file: &std::fs::File, buf: &[u8], mut offset: i64) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let fd = file.as_raw_fd();
    let mut written = 0usize;
    while written < buf.len() {
        // SAFETY: fd is open for the duration of the call, and
        // buf[written..] is a valid, initialized slice of the remaining
        // bytes to write.
        let n = unsafe {
            libc::pwrite(
                fd,
                buf[written..].as_ptr() as *const libc::c_void,
                buf.len() - written,
                offset,
            )
        };
        if n < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        written += n as usize;
        offset += n as i64;
    }
    Ok(())
}
