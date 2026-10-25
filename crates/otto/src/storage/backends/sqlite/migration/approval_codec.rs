//! The approval domain codec for native SQL staging.

use super::*;

/// Whether `approval_modes` still carries the pre-mode `automatic` column, so a
/// source database written before the mode form is exported with its meaning.
fn legacy_mode_column(connection: &Connection) -> Result<bool> {
    let mut statement = connection
        .prepare("PRAGMA table_info(approval_modes)")
        .map_err(sql_error)?;
    let fields = statement
        .query_map((), |row| row.get::<String>(1))
        .map_err(sql_error)?;
    for name in fields {
        if name.map_err(sql_error)? == "automatic" {
            return Ok(true);
        }
    }
    Ok(false)
}

pub struct ApprovalCodec;
use crate::approval::migration::ApprovalRecord;
impl SqlDomainCodec for ApprovalCodec {
    fn domain(&self) -> &'static str {
        "approvals"
    }
    fn family(&self) -> &'static str {
        "otto.approvals"
    }
    fn version(&self) -> u32 {
        1
    }
    fn committed_cursor(&self, connection: &Connection) -> Result<String> {
        connection
            .query_row(
                "SELECT COALESCE(MAX(sequence),0) FROM approval_events",
                (),
                |r| r.get::<i64>(0),
            )
            .map(|v| v.to_string())
            .map_err(sql_error)
    }
    fn initialize(&self, connection: &mut Connection) -> Result<()> {
        super::super::approval::initialize(connection).map_err(approval_error)
    }
    fn next_record(
        &self,
        connection: &Connection,
        after: &str,
        max_bytes: usize,
    ) -> Result<Option<(String, Record)>> {
        let (mut section, mut offset): (usize, usize) = if after.is_empty() {
            (0, 0)
        } else {
            let (section, offset) = after
                .split_once(':')
                .ok_or_else(|| Error::Invalid("invalid approval cursor".into()))?;
            (
                section
                    .parse()
                    .map_err(|_| Error::Invalid("invalid section".into()))?,
                offset
                    .parse()
                    .map_err(|_| Error::Invalid("invalid offset".into()))?,
            )
        };
        // A source written before the mode form stores `automatic` instead.
        let legacy = legacy_mode_column(connection)?;
        let canonical = connection.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='approval_settings'",(),|r|r.get::<i64>(0)).map_err(sql_error)? != 0;
        // ponytail: OFFSET scans are quadratic for large snapshots; switch these
        // five known record kinds to keyset cursors when migration volume needs it.
        while section < 8 {
            if section >= 5 && !canonical {
                section += 1;
                continue;
            }
            let (fields, lengths, table, order) = match section {
                7 => (
                    "workspace,body",
                    "length(CAST(workspace AS BLOB))+length(CAST(body AS BLOB))",
                    "approval_managed_policy",
                    "workspace",
                ),
                0 if canonical => (
                    "workspace,session_id,policy,reviewer,revision",
                    "length(CAST(workspace AS BLOB))+length(CAST(session_id AS BLOB))+64",
                    "approval_settings",
                    "workspace,session_id",
                ),
                0 => (
                    if legacy {
                        "workspace,session_id,automatic"
                    } else {
                        "workspace,session_id,mode"
                    },
                    "length(CAST(workspace AS BLOB))+length(CAST(session_id AS BLOB))+8",
                    "approval_modes",
                    "workspace,session_id",
                ),
                1 => (
                    "subject_key,body",
                    "length(CAST(subject_key AS BLOB))+length(CAST(body AS BLOB))",
                    "approvals",
                    "id",
                ),
                2 => (
                    "approval_id,workspace,session_id",
                    "length(CAST(approval_id AS BLOB))+length(CAST(workspace AS BLOB))+length(CAST(session_id AS BLOB))",
                    "approval_visibility",
                    "approval_id,workspace,session_id",
                ),
                3 => (
                    "approval_id,recovery",
                    "length(CAST(approval_id AS BLOB))+8",
                    "approval_claims",
                    "approval_id",
                ),
                4 => (
                    "sequence,body",
                    "length(CAST(body AS BLOB))+20",
                    "approval_events",
                    "sequence",
                ),
                5 => (
                    "workspace,session_id,evidence_digest,policy_digest",
                    "length(CAST(workspace AS BLOB))+length(CAST(session_id AS BLOB))+length(CAST(evidence_digest AS BLOB))+length(CAST(policy_digest AS BLOB))",
                    "approval_review_contexts",
                    "workspace,session_id",
                ),
                6 => (
                    "workspace,session_id,task_id,run_id,evidence_digest,denials",
                    "length(CAST(workspace AS BLOB))+length(CAST(session_id AS BLOB))+length(CAST(task_id AS BLOB))+length(CAST(run_id AS BLOB))+length(CAST(evidence_digest AS BLOB))+32",
                    "approval_review_denials",
                    "workspace,session_id,task_id,run_id,evidence_digest",
                ),
                _ => unreachable!(),
            };
            let length: Option<i64> = connection
                .query_row(
                    &format!("SELECT {lengths} FROM {table} ORDER BY {order} LIMIT 1 OFFSET ?1"),
                    [offset],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql_error)?;
            let Some(length) = length else {
                section += 1;
                offset = 0;
                continue;
            };
            // JSON can expand every byte to six bytes, then the neutral payload
            // Vec<u8> encoding expands each byte to four. Bound before loading.
            if length < 0
                || (length as usize)
                    .checked_mul(24)
                    .and_then(|n| n.checked_add(1024))
                    .is_none_or(|n| n > max_bytes)
            {
                return Err(Error::Invalid(
                    "approval record exceeds migration chunk limit".into(),
                ));
            }
            let record = connection
                .query_row(
                    &format!("SELECT {fields} FROM {table} ORDER BY {order} LIMIT 1 OFFSET ?1"),
                    [offset],
                    |r| {
                        let value = match section {
                            7 => {
                                let forced: otto_core::config::ApprovalConfig =
                                    serde_json::from_str(&r.get::<String>(1)?).map_err(|_| {
                                        crate::storage::Error::ConversionFailure(
                                            "invalid managed approval policy".into(),
                                        )
                                    })?;
                                ApprovalRecord::ManagedPolicy {
                                    workspace: r.get(0)?,
                                    policy: forced.policy,
                                    reviewer: forced.reviewer,
                                }
                            }
                            0 if canonical => ApprovalRecord::Settings {
                                workspace: r.get(0)?,
                                session_id: r.get(1)?,
                                settings: otto_core::approval::ApprovalSettings {
                                    policy: serde_json::from_value(serde_json::Value::String(
                                        r.get(2)?,
                                    ))
                                    .map_err(|_| {
                                        crate::storage::Error::ConversionFailure(
                                            "unknown approval policy".into(),
                                        )
                                    })?,
                                    reviewer: serde_json::from_value(serde_json::Value::String(
                                        r.get(3)?,
                                    ))
                                    .map_err(|_| {
                                        crate::storage::Error::ConversionFailure(
                                            "unknown approval reviewer".into(),
                                        )
                                    })?,
                                    revision: r.get(4)?,
                                },
                            },
                            0 => {
                                ApprovalRecord::Mode {
                                    workspace: r.get(0)?,
                                    session_id: r.get(1)?,
                                    mode: crate::approval::migration::StoredMode::Mode {
                                        mode: if legacy {
                                            // `automatic` was an integer boolean.
                                            match r.get::<i64>(2)? {
                                                0 => otto_core::approval::ApprovalMode::Manual,
                                                _ => otto_core::approval::ApprovalMode::Auto,
                                            }
                                        } else {
                                            otto_core::approval::ApprovalMode::parse(
                                                &r.get::<String>(2)?,
                                            )
                                            .ok_or_else(|| {
                                                crate::storage::Error::ConversionFailure(
                                                    "unknown approval mode".into(),
                                                )
                                            })?
                                        },
                                    },
                                }
                            }
                            1 => ApprovalRecord::Request {
                                subject_key: r.get(0)?,
                                request: r.get(1)?,
                            },
                            2 => ApprovalRecord::Visibility {
                                approval_id: r.get(0)?,
                                workspace: r.get(1)?,
                                session_id: r.get(2)?,
                            },
                            3 => ApprovalRecord::InterruptedClaim {
                                approval_id: r.get(0)?,
                                recovery: r.get(1)?,
                            },
                            4 => ApprovalRecord::Event {
                                sequence: r.get(0)?,
                                request: r.get(1)?,
                            },
                            5 => ApprovalRecord::ReviewContext {
                                workspace: r.get(0)?,
                                session_id: r.get(1)?,
                                evidence_digest: r.get(2)?,
                                policy_digest: r.get(3)?,
                            },
                            6 => ApprovalRecord::ReviewDenials {
                                workspace: r.get(0)?,
                                session_id: r.get(1)?,
                                task_id: r.get(2)?,
                                run_id: r.get(3)?,
                                evidence_digest: r.get(4)?,
                                denials: r.get(5)?,
                            },
                            _ => unreachable!(),
                        };
                        Ok(value)
                    },
                )
                .map_err(sql_error)?;
            let (id, revision) = record.metadata()?;
            let payload = serde_json::to_vec(&record).map_err(json_error)?;
            return Ok(Some((
                format!("{section}:{}", offset + 1),
                Record {
                    id,
                    revision,
                    payload,
                },
            )));
        }
        if section != 8 {
            return Err(Error::Invalid("unsupported approval cursor section".into()));
        }
        Ok(None)
    }
    fn import_record(&self, connection: &Connection, record: &Record) -> Result<()> {
        let value: ApprovalRecord = serde_json::from_slice(&record.payload).map_err(json_error)?;
        let expected = value.metadata()?;
        if record.id != expected.0 || record.revision != expected.1 {
            return Err(Error::Invalid(
                "logical approval identity/revision mismatch".into(),
            ));
        }
        match value {
            ApprovalRecord::ManagedPolicy {
                workspace,
                policy,
                reviewer,
            } => {
                let body = serde_json::to_string(&otto_core::config::ApprovalConfig {
                    policy,
                    reviewer,
                    ..Default::default()
                })
                .map_err(json_error)?;
                connection
                    .execute(
                        "INSERT INTO approval_managed_policy VALUES (?1,?2)",
                        params![workspace, body],
                    )
                    .map_err(sql_error)?;
            }
            ApprovalRecord::Settings {
                workspace,
                session_id,
                settings,
            } => {
                let policy = serde_json::to_value(settings.policy).map_err(json_error)?;
                let reviewer = serde_json::to_value(settings.reviewer).map_err(json_error)?;
                connection
                    .execute(
                        "INSERT INTO approval_settings VALUES (?1,?2,?3,?4,?5)",
                        params![
                            workspace,
                            session_id,
                            policy.as_str().unwrap(),
                            reviewer.as_str().unwrap(),
                            settings.revision
                        ],
                    )
                    .map_err(sql_error)?;
                connection
                    .execute(
                        "INSERT INTO approval_modes VALUES (?1,?2,?3)",
                        params![workspace, session_id, settings.mode().as_str()],
                    )
                    .map_err(sql_error)?;
            }
            ApprovalRecord::ReviewContext {
                workspace,
                session_id,
                evidence_digest,
                policy_digest,
            } => {
                if workspace.is_empty()
                    || (session_id.is_empty() && evidence_digest != "policy")
                    || evidence_digest.is_empty()
                    || policy_digest.is_empty()
                    || evidence_digest.len() > 256
                    || policy_digest.len() > 256
                {
                    return Err(Error::Invalid("invalid approval review context".into()));
                }
                connection
                    .execute(
                        "INSERT INTO approval_review_contexts VALUES (?1,?2,?3,?4)",
                        params![workspace, session_id, evidence_digest, policy_digest],
                    )
                    .map_err(sql_error)?;
            }
            ApprovalRecord::ReviewDenials {
                workspace,
                session_id,
                task_id,
                run_id,
                evidence_digest,
                denials,
            } => {
                if workspace.is_empty()
                    || session_id.is_empty()
                    || evidence_digest.is_empty()
                    || evidence_digest.len() > 256
                {
                    return Err(Error::Invalid("invalid approval denial history".into()));
                }
                connection
                    .execute(
                        "INSERT INTO approval_review_denials VALUES (?1,?2,?3,?4,?5,?6)",
                        params![
                            workspace,
                            session_id,
                            task_id,
                            run_id,
                            evidence_digest,
                            denials
                        ],
                    )
                    .map_err(sql_error)?;
            }
            ApprovalRecord::Mode {
                workspace,
                session_id,
                mode,
            } => {
                let mode: otto_core::approval::ApprovalMode = mode.into();
                connection
                    .execute(
                        "INSERT INTO approval_modes VALUES (?1,?2,?3)",
                        params![workspace, session_id, mode.as_str()],
                    )
                    .map_err(sql_error)?;
                let settings = otto_core::approval::ApprovalSettings::from_mode(mode);
                let reviewer = serde_json::to_value(settings.reviewer).map_err(json_error)?;
                connection
                    .execute(
                        "INSERT INTO approval_settings VALUES (?1,?2,'on_request',?3,1)",
                        params![workspace, session_id, reviewer.as_str().unwrap()],
                    )
                    .map_err(sql_error)?;
            }
            ApprovalRecord::Request {
                subject_key,
                request,
            } => {
                let body = request;
                let request = approval_request(&body)?;
                if record.id != format!("request:{}", request.id)
                    || record.revision != request.revision.to_string()
                {
                    return Err(Error::Invalid("approval identity/revision mismatch".into()));
                }
                let state = serde_json::to_value(request.state).map_err(json_error)?;
                connection
                    .execute(
                        "INSERT INTO approvals VALUES (?1,?2,?3,?4,?5,?6)",
                        params![
                            request.id,
                            subject_key,
                            request.scope.workspace,
                            request.scope.session_id,
                            state.as_str().unwrap(),
                            body
                        ],
                    )
                    .map_err(sql_error)?;
            }
            ApprovalRecord::Visibility {
                approval_id,
                workspace,
                session_id,
            } => {
                connection
                    .execute(
                        "INSERT INTO approval_visibility VALUES (?1,?2,?3)",
                        params![approval_id, workspace, session_id],
                    )
                    .map_err(sql_error)?;
            }
            ApprovalRecord::InterruptedClaim {
                approval_id,
                recovery,
            } => {
                // Preserve intent and ensure recovery observes a dead owner.
                // The destination service claims ownership before reconciliation.
                connection.execute("INSERT INTO approval_claims VALUES (?1,'migration-interrupted',2147483647,'1970-01-01T00:00:00Z',?2)",params![approval_id,recovery]).map_err(sql_error)?;
            }
            ApprovalRecord::Event { sequence, request } => {
                let body = request;
                let request = approval_request(&body)?;
                if sequence <= 0
                    || record.id != format!("event:{sequence}")
                    || record.revision != request.revision.to_string()
                {
                    return Err(Error::Invalid(
                        "approval event identity/revision mismatch".into(),
                    ));
                }
                connection
                    .execute(
                        "INSERT INTO approval_events VALUES (?1,?2,?3,?4,?5)",
                        params![
                            sequence,
                            request.id,
                            request.scope.workspace,
                            request.scope.session_id,
                            body
                        ],
                    )
                    .map_err(sql_error)?;
            }
        }
        Ok(())
    }
    fn validate(&self, connection: &Connection) -> Result<()> {
        let version: i64 = connection
            .query_row("PRAGMA user_version", (), |r| r.get(0))
            .map_err(sql_error)?;
        if !(2..=4).contains(&version) {
            return Err(Error::Invalid(
                "unsupported approval database schema".into(),
            ));
        }
        let unknown: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT IN \
            ('approval_modes','approval_settings','approval_review_contexts',\
            'approval_review_leases','approval_review_denials','approval_managed_policy',\
            'approvals','approval_events','approval_claims','approval_visibility',\
            'sqlite_sequence',\
            '__turso_internal_seq___turso_internal_autoincrement_approval_events',\
            'otto_migration_stage','otto_migration_chunks')",
                (),
                |r| r.get(0),
            )
            .map_err(sql_error)?;
        if unknown != 0 {
            return Err(Error::Invalid(
                "unsupported approval state tables; refusing partial migration".into(),
            ));
        }
        // Compatibility: approvals.db files written by earlier releases (Turso
        // engine) hold this physical sequence table for the AUTOINCREMENT
        // column; SQLite itself uses `sqlite_sequence`. Its contents are rebuilt by importing explicit event
        // IDs; it is not an independently owned domain record.
        let sequence_schema: Option<String> = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND \
            name='__turso_internal_seq___turso_internal_autoincrement_approval_events' AND \
            length(sql)<=4096",
                (),
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_error)?;
        if let Some(actual) = sequence_schema {
            let expected = "CREATE TABLE \
                \"__turso_internal_seq___turso_internal_autoincrement_approval_events\"(value INTEGER \
                PRIMARY KEY,is_called INTEGER,start INTEGER,inc INTEGER,min INTEGER,max \
                INTEGER,cycle INTEGER)";
            if normalize_schema_sql(&actual) != normalize_schema_sql(expected) {
                return Err(Error::Invalid(
                    "unsupported approval event sequence schema".into(),
                ));
            }
            let invalid: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM \
                __turso_internal_seq___turso_internal_autoincrement_approval_events \
                WHERE value<1 OR is_called NOT IN (0,1) OR start<>1 OR inc<>1 OR min<>1 \
                OR max<>9223372036854775807 OR cycle<>0 OR is_called IS NULL OR start \
                IS NULL OR inc IS NULL OR min IS NULL OR max IS NULL OR cycle IS NULL",
                    (),
                    |row| row.get(0),
                )
                .map_err(sql_error)?;
            if invalid != 0 {
                return Err(Error::Invalid(
                    "unsupported approval event sequence configuration".into(),
                ));
            }
        } else {
            let exists: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE \
                name='__turso_internal_seq___turso_internal_autoincrement_approval_events'",
                    (),
                    |row| row.get(0),
                )
                .map_err(sql_error)?;
            if exists != 0 {
                return Err(Error::Invalid(
                    "invalid approval event sequence schema".into(),
                ));
            }
        }
        let broken: i64 = connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM approval_visibility v LEFT JOIN approvals \
            a ON a.id=v.approval_id WHERE a.id IS NULL) + (SELECT COUNT(*) FROM \
            approval_claims c LEFT JOIN approvals a ON a.id=c.approval_id WHERE \
            a.id IS NULL) + (SELECT COUNT(*) FROM approval_events e LEFT JOIN \
            approvals a ON a.id=e.approval_id WHERE a.id IS NULL)",
                (),
                |r| r.get(0),
            )
            .map_err(sql_error)?;
        if broken != 0 {
            return Err(Error::Invalid("approval references are incomplete".into()));
        }
        let oversized: i64 = connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM approvals WHERE length(CAST(body AS BLOB))>?1)+(SELECT COUNT(*) FROM approval_events WHERE length(CAST(body AS BLOB))>?1)",
                [SCAN_LIMIT / 24],
                |r| r.get(0),
            )
            .map_err(sql_error)?;
        if oversized != 0 {
            return Err(Error::Invalid(
                "approval record exceeds validation bound".into(),
            ));
        }
        let mut statement = connection
            .prepare("SELECT id,workspace,session_id,state,body FROM approvals")
            .map_err(sql_error)?;
        for row in statement
            .query_map((), |r| {
                Ok((
                    r.get::<String>(0)?,
                    r.get::<String>(1)?,
                    r.get::<String>(2)?,
                    r.get::<String>(3)?,
                    r.get::<String>(4)?,
                ))
            })
            .map_err(sql_error)?
        {
            let (id, workspace, session, state, body) = row.map_err(sql_error)?;
            let request = approval_request(&body)?;
            if request.id != id
                || request.scope.workspace != workspace
                || request.scope.session_id != session
                || serde_json::to_value(request.state)
                    .map_err(json_error)?
                    .as_str()
                    != Some(&state)
            {
                return Err(Error::Invalid(
                    "approval projection differs from canonical record".into(),
                ));
            }
        }
        Ok(())
    }
    fn reopen(&self, connection: Connection) -> Result<()> {
        super::super::approval::Store::from_connection(connection)
            .map(|_| ())
            .map_err(approval_error)
    }
}
fn approval_error(error: crate::approval::Error) -> Error {
    match error {
        crate::approval::Error::CommitUnknown => Error::CommitUnknown,
        crate::approval::Error::Invalid(message) => Error::Invalid(message.into()),
        other => Error::Unavailable(other.to_string()),
    }
}

pub(super) fn approval_request(value: &str) -> Result<otto_core::approval::Request> {
    super::super::approval::decode_persisted(value).map_err(approval_error)
}
