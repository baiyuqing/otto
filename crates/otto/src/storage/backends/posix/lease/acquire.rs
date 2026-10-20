//! Lease acquisition: epoch markers, takeover and fenced-log handling.

use super::*;

/// Acquires `session_path`'s lease, using `clock` for the liveness-window
/// wait and `keeper` to register the lease once held. Implements the
/// protocol: with no epoch file, create epoch 1; otherwise read the
/// heartbeat once. A missing heartbeat waits, since it may belong to a
/// concurrent acquirer that has not written its first heartbeat yet. A
/// released heartbeat is taken over immediately only when its epoch is the
/// highest epoch marker present; a released heartbeat behind a newer marker
/// waits, for the same reason. Otherwise poll the heartbeat's raw bytes once
/// a second for 7`L`/6 — any change means the holder is live and acquisition
/// fails, no change for the whole window means it takes over. `EEXIST` on
/// the epoch create means a concurrent acquirer won the race.
pub(crate) fn acquire_with(
    session_path: &Path,
    clock: &dyn Clock,
    keeper: &Arc<Keeper>,
) -> Result<(Arc<Lease>, Acquired), AcquireError> {
    let dir_path = lease_dir(session_path);
    let dir = fsops::open_dir_no_follow(&dir_path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            AcquireError::Invalid(format!(
                "{} does not exist; call create_lease_dir first",
                dir_path.display()
            ))
        } else {
            AcquireError::Io(e)
        }
    })?;

    let meta = read_lease_meta(&dir_path)?;
    let l = Duration::from_secs(meta.lease_seconds);
    let highest = highest_epoch(&dir_path)?;

    if highest == 0 {
        return match create_epoch_marker(&dir, 1) {
            Ok(()) => finish_acquire(dir, 1, meta.lease_seconds, keeper, Acquired::Fresh),
            Err(e) if is_eexist(&e) => Err(AcquireError::Raced(1)),
            Err(e) => Err(AcquireError::Io(e)),
        };
    }

    let heartbeat = read_heartbeat(&dir)?;
    // A missing heartbeat waits out the same liveness window as a live one,
    // rather than being treated as an immediate takeover: the epoch marker
    // this process just saw may belong to a concurrent acquirer that has
    // not yet written its first heartbeat (`finish_acquire` writes the
    // marker, then the heartbeat, as two separate steps). Waiting gives
    // that heartbeat a chance to appear; only a marker whose heartbeat
    // never appears within the window is treated as an abandoned epoch.
    //
    // A released heartbeat is only trusted when it belongs to the highest
    // epoch marker: a released heartbeat behind a newer marker means a
    // concurrent acquirer already created that marker but has not written
    // its heartbeat yet, so waiting is needed here too, or two acquirers
    // would both take over and append to the log concurrently.
    let should_wait = !matches!(&heartbeat, Some(h) if h.released && h.epoch == highest);

    let mut prior_holder = heartbeat.as_ref().map(|h| Holder {
        epoch: highest,
        host: h.host.clone(),
        pid: h.pid,
    });

    if should_wait {
        let initial = read_heartbeat_bytes_lenient(&dir)?;
        let deadline = clock.now() + (l * 7 / 6);
        loop {
            if clock.now() >= deadline {
                break;
            }
            clock.sleep(Duration::from_secs(1));
            let current = read_heartbeat_bytes_lenient(&dir)?;
            if current != initial {
                if current.is_empty() {
                    // The heartbeat that appeared at `initial` was removed;
                    // nothing more to compare against.
                    continue;
                }
                let (host, pid) = parse_heartbeat_lenient(&current);
                return Err(AcquireError::Held(Holder {
                    epoch: highest,
                    host,
                    pid,
                }));
            }
        }
        // The window elapsed with no observed change. A heartbeat that
        // appeared during the wait (from a concurrent acquirer finishing
        // its own acquisition) but then stayed identical belongs to the
        // holder being taken over now.
        if !initial.is_empty() {
            let (host, pid) = parse_heartbeat_lenient(&initial);
            prior_holder = Some(Holder {
                epoch: highest,
                host,
                pid,
            });
        }
    }

    let next = highest + 1;
    match create_epoch_marker(&dir, next) {
        Ok(()) if should_wait => {
            // Taking over an unreleased epoch: the prior holder may still
            // hold a descriptor open on the session log, so its tail may
            // hold an incomplete record. Move it aside before any new write
            // reaches the log under this epoch.
            let holder = prior_holder.unwrap_or(Holder {
                epoch: highest,
                host: "unknown".into(),
                pid: 0,
            });
            if let Err(e) = move_aside_log(session_path, &dir_path, highest) {
                return Err(AcquireError::Io(e));
            }
            finish_acquire(
                dir,
                next,
                meta.lease_seconds,
                keeper,
                Acquired::TakenOver(holder),
            )
        }
        Ok(()) => {
            // The prior holder reported itself released; its log is already
            // complete, so there is nothing to move aside.
            finish_acquire(dir, next, meta.lease_seconds, keeper, Acquired::Fresh)
        }
        Err(e) if is_eexist(&e) => Err(AcquireError::Raced(next)),
        Err(e) => Err(AcquireError::Io(e)),
    }
}

/// Finds the highest-numbered `fenced-<n>.jsonl` in `dir_path`, or `None` if
/// none exists.
pub(super) fn highest_fenced_path(dir_path: &Path) -> std::io::Result<Option<PathBuf>> {
    let mut highest: Option<(u64, PathBuf)> = None;
    for entry in std::fs::read_dir(dir_path)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(rest) = name.strip_prefix("fenced-") else {
            continue;
        };
        let Some(rest) = rest.strip_suffix(".jsonl") else {
            continue;
        };
        if let Ok(n) = rest.parse::<u64>()
            && highest.as_ref().is_none_or(|(h, _)| n > *h)
        {
            highest = Some((n, entry.path()));
        }
    }
    Ok(highest.map(|(_, path)| path))
}

/// Moves `session_path`'s log aside when taking over epoch `epoch` from a
/// holder that had not released it, so the new epoch starts from a log
/// holding only complete records: renames the old log into the lease
/// directory as `fenced-<epoch>.jsonl`, then writes a copy of it, truncated
/// to its last complete line, back to `session_path` via a temp file and
/// rename. Rejects a source over
/// [`otto_core::session::MAX_SESSION_FILE_BYTES`] without reading it, so an
/// oversized log is left untouched rather than silently truncated. When
/// `session_path` is already missing -- because an earlier attempt got as
/// far as the first rename before being interrupted, possibly at a
/// different epoch than the one being retried now -- the highest-numbered
/// `fenced-*.jsonl` present is reused as the source instead of failing, so a
/// retried acquire finishes the same move; only when no `fenced-*.jsonl`
/// exists at all does this return `Ok` with nothing done. Fsyncs both the
/// lease directory and `session_path`'s parent directory before returning,
/// so both directories' entry changes are durable.
pub(super) fn move_aside_log(
    session_path: &Path,
    dir_path: &Path,
    epoch: u64,
) -> std::io::Result<()> {
    let fenced_path = dir_path.join(format!("fenced-{epoch}.jsonl"));

    let source_path = match std::fs::rename(session_path, &fenced_path) {
        Ok(()) => fenced_path,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && fenced_path.exists() => {
            // A previous attempt already moved the log aside at this epoch.
            fenced_path
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // A previous attempt may have moved the log aside under a
            // different epoch before crashing. Fall back to the highest
            // fenced copy present; if none exists, there was never a log.
            match highest_fenced_path(dir_path)? {
                Some(path) => path,
                None => return Ok(()),
            }
        }
        Err(e) => return Err(e),
    };

    let len = std::fs::metadata(&source_path)?.len();
    let max = otto_core::session::MAX_SESSION_FILE_BYTES as u64;
    if len > max {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{} is {len} bytes, over the {max}-byte session file limit",
                source_path.display()
            ),
        ));
    }

    let mut bytes = std::fs::read(&source_path)?;
    let complete_len = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    bytes.truncate(complete_len);

    let suffix = crate::auth::random_hex::<8>()?;
    let temp_path = dir_path.join(format!("restore.tmp-{suffix}"));
    let write_result = (|| -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        // 0600, matching the mode `Store` creates session logs with.
        let mut temp = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC)
            .open(&temp_path)?;
        temp.write_all(&bytes)?;
        temp.sync_all()?;
        std::fs::rename(&temp_path, session_path)?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(e);
    }

    std::fs::File::open(dir_path)?.sync_all()?;
    if let Some(parent) = session_path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }

    Ok(())
}

pub(super) fn finish_acquire(
    dir: fsops::Dir,
    epoch: u64,
    lease_seconds: u64,
    keeper: &Arc<Keeper>,
    acquired: Acquired,
) -> Result<(Arc<Lease>, Acquired), AcquireError> {
    let lease = Arc::new(Lease {
        _keeper: Arc::clone(keeper),
        epoch,
        host: local_hostname(),
        pid: std::process::id(),
        duration_secs: lease_seconds,
        dir,
        write_lock: Mutex::new(()),
        last_renewal: Mutex::new(Some(Instant::now())),
        seq: AtomicU64::new(0),
        fenced: AtomicBool::new(false),
        superseded: AtomicBool::new(false),
        released: AtomicBool::new(false),
        lost_reason: Mutex::new(None),
        fail_renewal: AtomicBool::new(false),
    });
    lease.write_heartbeat(false).map_err(AcquireError::Io)?;
    Keeper::register(keeper, &lease);
    Ok((lease, acquired))
}
