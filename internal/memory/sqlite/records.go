package sqlite

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"math"
	"strconv"
	"strings"

	"github.com/baiyuqing/otto/internal/memory"
)

var recordProjectionSafety = buildRecordProjectionSafety()

func buildRecordProjectionSafety() string {
	text := func(column string, minimum, maximum int) string {
		return fmt.Sprintf("typeof(%s)='text' AND length(CAST(%s AS BLOB)) BETWEEN %d AND %d", column, column, minimum, maximum)
	}
	bounded := func(column string, maximum int) string { return text(column, 0, maximum) }
	jsonValue := func(column string, maximum int, kind string) string {
		gate := bounded(column, maximum)
		projected := "CASE WHEN " + gate + " THEN " + column + " END"
		return gate + " AND json_valid(" + projected + ") AND json_type(" + projected + ")='" + kind + "'"
	}
	return strings.Join([]string{
		text("id", 1, memory.MaxIDBytes), text("scope_namespace", 1, memory.MaxNamespaceBytes),
		text("scope_id", 1, memory.MaxScopeIDBytes), text("kind", 1, memory.MaxKindBytes),
		bounded("semantic_key", memory.MaxSemanticKeyBytes), text("text_value", 1, memory.MaxRecordTextBytes),
		jsonValue("labels_json", maxLabelsJSONBytes, "array"), jsonValue("metadata_json", memory.MaxMetadataBytes, "object"),
		jsonValue("source_json", maxSourceJSONBytes, "object"),
		"typeof(confidence) IN ('real','integer')", "typeof(revision)='integer' AND revision BETWEEN 1 AND " + strconv.FormatInt(math.MaxInt64, 10),
		text("created_at", len(timestampLayout), len(timestampLayout)), text("updated_at", len(timestampLayout), len(timestampLayout)),
		"(expires_at IS NULL OR (" + text("expires_at", len(timestampLayout), len(timestampLayout)) + "))",
		"typeof(state)='text' AND state='active'", "forgotten_at IS NULL",
	}, " AND ")
}

func recordProjection() string {
	columns := []string{
		"id", "scope_namespace", "scope_id", "kind", "semantic_key", "text_value",
		"labels_json", "metadata_json", "source_json", "confidence", "revision",
		"created_at", "updated_at", "expires_at",
	}
	parts := make([]string, 0, len(columns)+1)
	parts = append(parts, "CASE WHEN "+recordProjectionSafety+" THEN 1 ELSE 0 END")
	for _, column := range columns {
		parts = append(parts, "CASE WHEN "+recordProjectionSafety+" THEN "+column+" END")
	}
	return strings.Join(parts, ",")
}

type rowScanner interface{ Scan(...any) error }

func decodeRecordRow(scanner rowScanner) (memory.Record, error) {
	var valid sql.NullInt64
	var id, namespace, scopeID, kind, key, text sql.NullString
	var labelsJSON, metadataJSON, sourceJSON sql.NullString
	var confidence sql.NullFloat64
	var revision sql.NullInt64
	var createdAt, updatedAt, expiresAt sql.NullString
	if err := scanner.Scan(
		&valid, &id, &namespace, &scopeID, &kind, &key, &text,
		&labelsJSON, &metadataJSON, &sourceJSON, &confidence, &revision,
		&createdAt, &updatedAt, &expiresAt,
	); err != nil {
		return memory.Record{}, err
	}
	mandatoryStrings := [...]sql.NullString{id, namespace, scopeID, kind, key, text, labelsJSON, metadataJSON, sourceJSON, createdAt, updatedAt}
	if !valid.Valid || valid.Int64 != 1 || !confidence.Valid || !revision.Valid {
		return memory.Record{}, memory.ErrCorrupt
	}
	for _, value := range mandatoryStrings {
		if !value.Valid {
			return memory.Record{}, memory.ErrCorrupt
		}
	}
	labels, err := decodeLabels([]byte(labelsJSON.String))
	if err != nil {
		return memory.Record{}, memory.ErrCorrupt
	}
	metadata, err := decodeMetadata([]byte(metadataJSON.String))
	if err != nil {
		return memory.Record{}, memory.ErrCorrupt
	}
	source, err := decodeProvenance([]byte(sourceJSON.String))
	if err != nil {
		return memory.Record{}, memory.ErrCorrupt
	}
	created, err := parseTimestamp(createdAt.String)
	if err != nil {
		return memory.Record{}, memory.ErrCorrupt
	}
	updated, err := parseTimestamp(updatedAt.String)
	if err != nil {
		return memory.Record{}, memory.ErrCorrupt
	}
	record := memory.Record{
		ID: id.String, Scope: memory.Scope{Namespace: namespace.String, ID: scopeID.String},
		Kind: kind.String, Key: key.String, Text: text.String, Labels: labels, Metadata: metadata,
		Source: source, Confidence: confidence.Float64, Revision: uint64(revision.Int64), CreatedAt: created, UpdatedAt: updated,
	}
	if expiresAt.Valid {
		value, err := parseTimestamp(expiresAt.String)
		if err != nil {
			return memory.Record{}, memory.ErrCorrupt
		}
		record.ExpiresAt = &value
	}
	if !validStoredFloat(record.Confidence) || memory.ValidateRecord(record) != nil {
		return memory.Record{}, memory.ErrCorrupt
	}
	return record, nil
}

func (store *Store) Get(ctx context.Context, ref memory.RecordRef) (memory.Record, error) {
	if err := ctx.Err(); err != nil {
		return memory.Record{}, err
	}
	if err := memory.ValidateRecordRef(ref); err != nil {
		return memory.Record{}, err
	}
	return store.getRecord(ctx, `scope_namespace=? AND scope_id=? AND id=?`, ref.Scope.Namespace, ref.Scope.ID, ref.ID)
}

func (store *Store) GetByKey(ctx context.Context, key memory.RecordKey) (memory.Record, error) {
	if err := ctx.Err(); err != nil {
		return memory.Record{}, err
	}
	if err := memory.ValidateRecordKey(key); err != nil {
		return memory.Record{}, err
	}
	return store.getRecord(ctx, `scope_namespace=? AND scope_id=? AND kind=? AND semantic_key=?`, key.Scope.Namespace, key.Scope.ID, key.Kind, key.Key)
}

func (store *Store) getRecord(ctx context.Context, predicate string, arguments ...any) (memory.Record, error) {
	done, err := store.admit()
	if err != nil {
		return memory.Record{}, err
	}
	conn, err := store.borrowConnection(ctx)
	if err != nil {
		done()
		return memory.Record{}, err
	}
	query := "SELECT " + recordProjection() + " FROM memory_records WHERE state='active' AND " + predicate + " LIMIT 1"
	record, scanErr := decodeRecordRow(conn.QueryRowContext(ctx, query, arguments...))
	store.returnConnection(conn)
	done()
	if scanErr != nil {
		if errors.Is(scanErr, sql.ErrNoRows) {
			return memory.Record{}, memory.ErrNotFound
		}
		return memory.Record{}, safeRecordReadError(ctx, scanErr)
	}
	if err := memory.GuardRecord(ctx, store.guard, record); err != nil {
		return memory.Record{}, err
	}
	return memory.CloneRecord(record), nil
}

func (store *Store) List(ctx context.Context, request memory.ListRequest) (memory.RecordPage, error) {
	if err := ctx.Err(); err != nil {
		return memory.RecordPage{}, err
	}
	request = memory.CloneListRequest(request)
	if err := memory.ValidateListRequest(request); err != nil {
		return memory.RecordPage{}, err
	}
	fingerprint, err := fingerprintList(request)
	if err != nil {
		return memory.RecordPage{}, memory.ErrInvalidCursor
	}
	cursor, cursorGeneration, err := decodeRecordCursor(request.Cursor, fingerprint)
	if err != nil {
		return memory.RecordPage{}, memory.ErrInvalidCursor
	}
	done, err := store.admit()
	if err != nil {
		return memory.RecordPage{}, err
	}
	conn, err := store.borrowConnection(ctx)
	if err != nil {
		done()
		return memory.RecordPage{}, err
	}
	contaminated, err := beginReadTransaction(ctx, conn)
	if err != nil {
		if contaminated {
			store.quarantine(conn)
		} else {
			store.returnConnection(conn)
		}
		done()
		return memory.RecordPage{}, err
	}
	generation, readErr := readGeneration(ctx, conn)
	if readErr == nil && request.Cursor != "" && generation != cursorGeneration {
		readErr = memory.ErrConflict
	}
	var records []memory.Record
	if readErr == nil && len(request.Scopes) != 0 {
		query, arguments := buildListQuery(request, cursor)
		rows, queryErr := conn.QueryContext(ctx, query, arguments...)
		if queryErr != nil {
			readErr = safeRecordReadError(ctx, queryErr)
		} else {
			for rows.Next() {
				if len(records) >= memory.MaxPageSize+1 {
					readErr = memory.ErrCorrupt
					break
				}
				record, decodeErr := decodeRecordRow(rows)
				if decodeErr != nil {
					readErr = safeRecordReadError(ctx, decodeErr)
					break
				}
				records = append(records, record)
			}
			if rowsErr := rows.Err(); readErr == nil && rowsErr != nil {
				readErr = safeRecordReadError(ctx, rowsErr)
			}
			if closeErr := rows.Close(); readErr == nil && closeErr != nil {
				readErr = safeRecordReadError(ctx, closeErr)
			}
		}
	}
	endErr := endReadTransaction(conn)
	if readErr == nil && endErr != nil {
		readErr = endErr
	}
	if endErr != nil {
		store.quarantine(conn)
	} else {
		store.returnConnection(conn)
	}
	done()
	if readErr != nil {
		return memory.RecordPage{}, readErr
	}
	for _, record := range records {
		if err := memory.GuardRecord(ctx, store.guard, record); err != nil {
			return memory.RecordPage{}, err
		}
	}
	page := memory.RecordPage{Records: records}
	if len(page.Records) > request.Limit {
		page.Records = page.Records[:request.Limit]
		last := page.Records[len(page.Records)-1]
		page.NextCursor, err = encodeRecordCursor(fingerprint, generation, formatTimestamp(last.UpdatedAt), last.ID)
		if err != nil {
			return memory.RecordPage{}, memory.ErrInvalidCursor
		}
	}
	return memory.CloneRecordPage(page), nil
}

func buildListQuery(request memory.ListRequest, cursor recordCursor) (string, []any) {
	clauses := []string{"state='active'"}
	arguments := make([]any, 0, len(request.Scopes)*2+len(request.Kinds)+len(request.Labels)+4)
	scopes := make([]string, 0, len(request.Scopes))
	for _, scope := range request.Scopes {
		scopes = append(scopes, "(scope_namespace=? AND scope_id=?)")
		arguments = append(arguments, scope.Namespace, scope.ID)
	}
	clauses = append(clauses, "("+strings.Join(scopes, " OR ")+")")
	if len(request.Kinds) != 0 {
		clauses = append(clauses, "kind IN ("+placeholders(len(request.Kinds))+")")
		for _, kind := range request.Kinds {
			arguments = append(arguments, kind)
		}
	}
	labelGate := "typeof(labels_json)='text' AND length(CAST(labels_json AS BLOB))<=" + strconv.Itoa(maxLabelsJSONBytes)
	gatedLabels := "CASE WHEN " + labelGate + " THEN labels_json END"
	for _, label := range request.Labels {
		clauses = append(clauses, "EXISTS (SELECT 1 FROM json_each(CASE WHEN json_valid("+gatedLabels+") THEN "+gatedLabels+" ELSE '[]' END) WHERE type='text' AND value=?)")
		arguments = append(arguments, label)
	}
	if !request.IncludeExpired {
		clauses = append(clauses, "(expires_at IS NULL OR expires_at>?)")
		arguments = append(arguments, formatTimestamp(request.Now))
	}
	if cursor.Version != 0 {
		clauses = append(clauses, "(updated_at<? OR (updated_at=? AND id>?))")
		arguments = append(arguments, cursor.UpdatedAt, cursor.UpdatedAt, cursor.ID)
	}
	arguments = append(arguments, request.Limit+1)
	query := "SELECT " + recordProjection() + " FROM memory_records WHERE " + strings.Join(clauses, " AND ") + " ORDER BY updated_at DESC,id ASC LIMIT ?"
	return query, arguments
}

func placeholders(count int) string { return strings.TrimSuffix(strings.Repeat("?,", count), ",") }

func (store *Store) Upsert(ctx context.Context, request memory.UpsertRequest) (memory.Record, error) {
	if err := ctx.Err(); err != nil {
		return memory.Record{}, err
	}
	request = memory.CloneUpsertRequest(request)
	if err := memory.ValidateUpsertRequest(request); err != nil {
		return memory.Record{}, err
	}
	if request.ExpectedRevision != nil {
		return memory.Record{}, memory.ErrUnsupported
	}
	if err := memory.GuardRecord(ctx, store.guard, request.Record); err != nil {
		return memory.Record{}, err
	}
	desired := memory.CloneRecord(request.Record)
	desired.Revision = 1
	encoded, err := encodeRecord(desired)
	if err != nil {
		return memory.Record{}, err
	}
	err = store.withWrite(ctx, memory.CommitUpsert, []string{desired.ID}, func(ctx context.Context, conn *sql.Conn) error {
		_, err := conn.ExecContext(ctx, `INSERT INTO memory_records(
			id,scope_namespace,scope_id,kind,semantic_key,text_value,labels_json,metadata_json,source_json,
			confidence,revision,created_at,updated_at,expires_at,state,forgotten_at
		) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,'active',NULL)`,
			desired.ID, desired.Scope.Namespace, desired.Scope.ID, desired.Kind, desired.Key, desired.Text,
			string(encoded.labels), string(encoded.metadata), string(encoded.source), desired.Confidence, 1,
			encoded.created, encoded.updated, encoded.expires,
		)
		if err != nil {
			return err
		}
		if _, err := conn.ExecContext(ctx, `INSERT INTO memory_records_fts(record_id,text_value,kind,semantic_key,labels) VALUES(?,?,?,?,?)`,
			desired.ID, desired.Text, desired.Kind, desired.Key, ftsLabels(desired.Labels)); err != nil {
			return err
		}
		return bumpGeneration(ctx, conn)
	})
	if err != nil {
		if errors.Is(err, memory.ErrConflict) {
			return memory.Record{}, &memory.ConflictError{EntityKind: "record", ID: desired.ID}
		}
		return memory.Record{}, err
	}
	return memory.CloneRecord(desired), nil
}

func readGeneration(ctx context.Context, conn *sql.Conn) (uint64, error) {
	var value sql.NullString
	err := conn.QueryRowContext(ctx, `SELECT CASE WHEN typeof(value)='text' AND length(CAST(value AS BLOB)) BETWEEN 1 AND 20 THEN value END FROM memory_meta WHERE key='generation' LIMIT 1`).Scan(&value)
	if err != nil || !value.Valid {
		if ctxErr := ctx.Err(); ctxErr != nil {
			return 0, ctxErr
		}
		return 0, memory.ErrCorrupt
	}
	generation, err := strconv.ParseUint(value.String, 10, 64)
	if err != nil || strconv.FormatUint(generation, 10) != value.String {
		return 0, memory.ErrCorrupt
	}
	return generation, nil
}

func bumpGeneration(ctx context.Context, conn *sql.Conn) error {
	generation, err := readGeneration(ctx, conn)
	if err != nil {
		return err
	}
	if generation == math.MaxUint64 {
		return memory.ErrCorrupt
	}
	result, err := conn.ExecContext(ctx, `UPDATE memory_meta SET value=? WHERE key='generation' AND value=?`, strconv.FormatUint(generation+1, 10), strconv.FormatUint(generation, 10))
	if err != nil {
		return err
	}
	changed, err := result.RowsAffected()
	if err != nil || changed != 1 {
		return memory.ErrCorrupt
	}
	return nil
}

func beginReadTransaction(ctx context.Context, conn *sql.Conn) (contaminated bool, result error) {
	if _, err := conn.ExecContext(ctx, "PRAGMA query_only=ON"); err != nil {
		return false, safeSQLiteError(ctx, err)
	}
	if _, err := conn.ExecContext(ctx, "BEGIN"); err != nil {
		if _, resetErr := conn.ExecContext(context.Background(), "PRAGMA query_only=OFF"); resetErr != nil {
			return true, memory.ErrUnavailable
		}
		return false, safeSQLiteError(ctx, err)
	}
	return false, nil
}

func endReadTransaction(conn *sql.Conn) error {
	if _, err := conn.ExecContext(context.Background(), "ROLLBACK"); err != nil {
		return memory.ErrUnavailable
	}
	if _, err := conn.ExecContext(context.Background(), "PRAGMA query_only=OFF"); err != nil {
		return memory.ErrUnavailable
	}
	return nil
}

func safeRecordReadError(ctx context.Context, err error) error {
	if err == nil {
		return nil
	}
	if ctxErr := ctx.Err(); ctxErr != nil {
		return ctxErr
	}
	if errors.Is(err, memory.ErrCorrupt) {
		return memory.ErrCorrupt
	}
	if errors.Is(err, memory.ErrConflict) {
		return memory.ErrConflict
	}
	if errors.Is(err, sql.ErrNoRows) {
		return memory.ErrNotFound
	}
	mapped := safeSQLiteError(ctx, err)
	if errors.Is(mapped, memory.ErrUnavailable) {
		// Scan conversion failures on a length-gated projection indicate an
		// externally corrupted STRICT row, not caller-visible driver detail.
		return memory.ErrCorrupt
	}
	return mapped
}
