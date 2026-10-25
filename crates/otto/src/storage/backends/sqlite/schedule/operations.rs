//! Public schedule operations: create, claim, finish, deliver and sweep.

use super::*;

impl SqliteScheduleStore {
    pub fn create(
        &self,
        item: NewItem,
        now: DateTime<Utc>,
    ) -> std::result::Result<Item, CreateError> {
        let id = self
            .write(|connection| {
                let active: i64 = connection.query_row(
                    "SELECT COUNT(*) FROM items WHERE origin = ?1 AND state IN ('waiting','ready')",
                    params![item.origin.as_str()],
                    |row| row.get(0),
                )?;
                if active >= MAX_ACTIVE_PER_ORIGIN {
                    return Ok(None);
                }
                connection.execute(
                    "INSERT INTO items (origin, session_id, workspace, kind, body, repeat, \
                     local_time, due_at, state, result, attempts, next_try_at, claimed_until, \
                     last_error, created_at, updated_at) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'waiting','',0,?8,0,'',?9,?9)",
                    params![
                        item.origin.as_str(),
                        item.session_id,
                        item.workspace,
                        item.kind.as_str(),
                        item.body,
                        item.repeat.map_or("", Repeat::as_str),
                        item.local_time
                            .map(|time| time.format("%H:%M").to_string())
                            .unwrap_or_default(),
                        ms(item.due_at),
                        ms(now),
                    ],
                )?;
                let id: i64 =
                    connection.query_row("SELECT last_insert_rowid()", (), |row| row.get(0))?;
                Ok(Some(id))
            })
            .map_err(CreateError::Store)?
            .ok_or(CreateError::Limit)?;
        let created = self
            .read(|connection| get(connection, id))
            .map_err(CreateError::Store)?
            .ok_or(CreateError::Store(Error))?;
        self.notify.notify_waiters();
        Ok(created)
    }

    /// Active items, then items that ended within 7 days.
    pub fn list(&self, origin: &Origin, now: DateTime<Utc>) -> Result<Vec<Item>> {
        self.read(|connection| {
            query_items(
                connection,
                &format!(
                    "SELECT {COLUMNS} FROM items WHERE origin = ?1 \
                     AND (state IN ('waiting','ready') OR updated_at > ?2) \
                     ORDER BY CASE WHEN state IN ('waiting','ready') THEN 0 ELSE 1 END, due_at, id"
                ),
                params![origin.as_str(), ms(now - RETENTION)],
            )
        })
    }

    /// `None` when `origin` has no active item `id`.
    pub fn cancel(&self, origin: &Origin, id: i64, now: DateTime<Utc>) -> Result<Option<Item>> {
        let cancelled = self.write(|connection| {
            let changed = connection.execute(
                "UPDATE items SET state = 'cancelled', claimed_until = 0, updated_at = ?3 \
                 WHERE id = ?1 AND origin = ?2 AND state IN ('waiting','ready')",
                params![id, origin.as_str(), ms(now)],
            )?;
            if changed == 0 {
                return Ok(None);
            }
            get(connection, id)
        })?;
        self.notify.notify_waiters();
        Ok(cancelled)
    }

    /// Expiry (24 h), recurring catch-up and advance, retention (7 days).
    /// Rows whose claim has not expired are left alone.
    pub fn sweep(&self, now: DateTime<Utc>) -> Result<()> {
        self.write(|connection| {
            let due = query_items(
                connection,
                &format!(
                    "SELECT {COLUMNS} FROM items WHERE state IN ('waiting','ready') \
                     AND due_at <= ?1 AND claimed_until <= ?1"
                ),
                params![ms(now)],
            )?;
            for item in due {
                let age = now - item.due_at;
                match recurrence(&item) {
                    None if age > EXPIRY => {
                        connection.execute(
                            "UPDATE items SET state = 'failed', last_error = ?2, updated_at = ?3 \
                             WHERE id = ?1",
                            params![item.id, EXPIRED, ms(now)],
                        )?;
                    }
                    None => {}
                    Some((repeat, time)) if item.state == State::Waiting => {
                        let latest = latest_occurrence(repeat, time, now, &Local);
                        if now - latest > EXPIRY {
                            advance(connection, &item, now, EXPIRED)?;
                        } else if latest > item.due_at {
                            connection.execute(
                                "UPDATE items SET due_at = ?2, next_try_at = ?2, attempts = 0, \
                                 updated_at = ?3 WHERE id = ?1",
                                params![item.id, ms(latest), ms(now)],
                            )?;
                        }
                    }
                    Some(_) if age > EXPIRY => advance(connection, &item, now, EXPIRED)?,
                    Some(_) => {}
                }
            }
            connection.execute(
                "DELETE FROM items WHERE state IN ('done','failed','cancelled') AND updated_at < ?1",
                params![ms(now - RETENTION)],
            )?;
            connection.execute(
                "DELETE FROM deliveries WHERE delivered_at < ?1",
                params![ms(now - RETENTION)],
            )?;
            Ok(())
        })
    }

    /// Claims one due `waiting` prompt item for `scope` (and `workspace`, when
    /// given) for 15 min.
    pub fn claim_run(
        &self,
        scope: &Scope,
        workspace: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<Option<Item>> {
        self.write(|connection| {
            let candidates = query_items(
                connection,
                &format!(
                    "SELECT {COLUMNS} FROM items WHERE state = 'waiting' AND kind = 'prompt' \
                     AND next_try_at <= ?1 AND claimed_until <= ?1 \
                     AND (?3 = 0 OR workspace = ?2) ORDER BY next_try_at, id LIMIT 50"
                ),
                params![
                    ms(now),
                    workspace.unwrap_or(""),
                    i64::from(workspace.is_some())
                ],
            )?;
            let Some(mut item) = candidates
                .into_iter()
                .find(|item| scope.matches(&item.origin))
            else {
                return Ok(None);
            };
            item.claimed_until = now + RUN_LEASE;
            connection.execute(
                "UPDATE items SET claimed_until = ?2 WHERE id = ?1",
                params![item.id, ms(item.claimed_until)],
            )?;
            Ok(Some(item))
        })
    }

    /// Records the result of the run of occurrence `(id, due_at)`. `false`
    /// when that occurrence is no longer current or the item was cancelled.
    pub fn finish_run(
        &self,
        id: i64,
        due_at: DateTime<Utc>,
        outcome: RunOutcome,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        self.write(|connection| {
            let Some(item) = current(connection, id, due_at, State::Waiting)? else {
                return Ok(false);
            };
            let failure = match outcome {
                RunOutcome::Reply(reply) => {
                    let reply = reply.trim();
                    let result = if reply.is_empty() {
                        format!(
                            "⏰ Scheduled prompt {} finished with no reply.",
                            item.label()
                        )
                    } else {
                        format!("⏰ {reply}")
                    };
                    make_ready(connection, id, &result, "", now)?;
                    return Ok(true);
                }
                RunOutcome::FailedBeforeTools(error) if now - item.due_at < EXPIRY => {
                    let attempts = item.attempts + 1;
                    connection.execute(
                        "UPDATE items SET attempts = ?2, next_try_at = ?3, last_error = ?4, \
                         claimed_until = 0, updated_at = ?5 WHERE id = ?1",
                        params![
                            id,
                            attempts,
                            ms(now + backoff(attempts)),
                            cut(&error, MAX_ERROR_CHARS),
                            ms(now)
                        ],
                    )?;
                    return Ok(true);
                }
                RunOutcome::FailedBeforeTools(error) | RunOutcome::Failed(error) => {
                    cut(&error, MAX_ERROR_CHARS)
                }
            };
            let result = format!("⏰ Scheduled prompt {} failed: {failure}", item.label());
            make_ready(connection, id, &result, &failure, now)?;
            Ok(true)
        })
    }

    /// Claims up to `limit` deliverable items for `scope`: `ready` ones, and
    /// due `waiting` text items, which become `ready` with result `⏰ <body>`.
    pub fn claim_deliveries(
        &self,
        scope: &Scope,
        lease: Duration,
        limit: usize,
        now: DateTime<Utc>,
    ) -> Result<Vec<Item>> {
        self.write(|connection| {
            let resolved;
            let scope = if let Scope::DeliveryClient(id) = scope {
                resolved = delivery_scope(connection, id, now)?.0;
                &resolved
            } else { scope };
            // ponytail: scan due rows for dynamic scopes; use SQL filters if queues grow.

            let candidates = query_items(
                connection,
                &format!(
                    "SELECT {COLUMNS} FROM items WHERE next_try_at <= ?1 AND claimed_until <= ?1 \
                     AND (state = 'ready' OR (state = 'waiting' AND kind = 'text')) \
                     ORDER BY next_try_at, id"
                ),
                params![ms(now)],
            )?;
            let claimed_until = now + TimeDelta::from_std(lease).unwrap_or(TimeDelta::minutes(3));
            let mut claimed = Vec::new();
            for mut item in candidates {
                if claimed.len() >= limit {
                    break;
                }
                if !scope.matches(&item.origin) {
                    continue;
                }
                if item.state == State::Waiting {
                    item.result = format!("⏰ {}", item.body);
                    item.state = State::Ready;
                }
                item.claimed_until = claimed_until;
                connection.execute(
                    "UPDATE items SET state = 'ready', result = ?2, claimed_until = ?3 WHERE id = ?1",
                    params![item.id, item.result, ms(claimed_until)],
                )?;
                claimed.push(item);
            }
            Ok(claimed)
        })
    }

    /// Records the delivery of occurrence `(id, due_at)` and advances a
    /// recurring item or sets it `done`. `false` when the occurrence is no
    /// longer current.
    pub fn delivered(&self, id: i64, due_at: DateTime<Utc>, now: DateTime<Utc>) -> Result<bool> {
        self.write(|connection| {
            let Some(item) = current(connection, id, due_at, State::Ready)? else {
                return Ok(false);
            };
            connection.execute(
                "INSERT INTO deliveries (item_id, due_at, origin, session_id, text, delivered_at, appended) \
                 VALUES (?1,?2,?3,?4,?5,?6,0) ON CONFLICT(item_id, due_at) DO NOTHING",
                params![
                    id,
                    ms(due_at),
                    item.origin.as_str(),
                    item.session_id,
                    item.result,
                    ms(now)
                ],
            )?;
            if recurrence(&item).is_some() {
                advance(connection, &item, now, "")?;
            } else {
                connection.execute(
                    "UPDATE items SET state = 'done', last_error = '', claimed_until = 0, \
                     updated_at = ?2 WHERE id = ?1",
                    params![id, ms(now)],
                )?;
            }
            Ok(true)
        })
    }

    /// `attempts + 1`, `next_try_at = now + backoff`, the claim released.
    pub fn delivery_failed(
        &self,
        id: i64,
        due_at: DateTime<Utc>,
        error: &str,
        now: DateTime<Utc>,
    ) -> Result<()> {
        self.write(|connection| {
            let Some(item) = current(connection, id, due_at, State::Ready)? else {
                return Ok(());
            };
            let attempts = item.attempts + 1;
            connection.execute(
                "UPDATE items SET attempts = ?2, next_try_at = ?3, last_error = ?4, \
                 claimed_until = 0, updated_at = ?5 WHERE id = ?1",
                params![
                    id,
                    attempts,
                    ms(now + backoff(attempts)),
                    cut(error, MAX_ERROR_CHARS),
                    ms(now)
                ],
            )?;
            Ok(())
        })
    }

    /// The earliest instant at which an active row becomes claimable, for
    /// worker sleeps.
    pub fn next_due(&self, _now: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
        self.read(|connection| {
            let earliest: Option<i64> = connection.query_row(
                "SELECT MIN(MAX(next_try_at, claimed_until)) FROM items \
                 WHERE state IN ('waiting','ready')",
                (),
                |row| row.get(0),
            )?;
            Ok(earliest.map(from_ms))
        })
    }

    /// Up to `limit` deliveries not yet written back, oldest first. For a chat
    /// origin: rows of that origin. For terminal and web: rows of
    /// `session_id` whose origin is not a chat.
    pub fn pending_writebacks(
        &self,
        origin: &Origin,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<Delivery>> {
        self.read(|connection| {
            let (filter, key) = if origin.is_chat() {
                ("origin = ?1", origin.as_str())
            } else {
                ("session_id = ?1 AND origin NOT LIKE 'chat:%'", session_id)
            };
            let mut statement = connection.prepare(&format!(
                "SELECT item_id, due_at, origin, session_id, text, delivered_at FROM deliveries \
                 WHERE appended = 0 AND {filter} ORDER BY delivered_at, item_id, due_at LIMIT ?2"
            ))?;
            statement
                .query_map(params![key, limit as i64], |row| {
                    let origin: String = row.get(2)?;
                    Ok(Delivery {
                        item_id: row.get(0)?,
                        due_at: from_ms(row.get(1)?),
                        origin: Origin::parse(&origin).map_err(|_| {
                            crate::storage::Error::ConversionFailure("bad origin".into())
                        })?,
                        session_id: row.get(3)?,
                        text: row.get(4)?,
                        delivered_at: from_ms(row.get(5)?),
                    })
                })?
                .collect()
        })
    }

    pub fn mark_appended(&self, item_id: i64, due_at: DateTime<Utc>) -> Result<()> {
        self.write(|connection| {
            connection.execute(
                "UPDATE deliveries SET appended = 1 WHERE item_id = ?1 AND due_at = ?2",
                params![item_id, ms(due_at)],
            )?;
            Ok(())
        })
    }
}
