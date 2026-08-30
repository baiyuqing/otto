package sqlite

import (
	"fmt"
	"math"
	"strconv"
	"strings"
	"unicode"
	"unicode/utf8"

	"github.com/baiyuqing/otto/internal/memory"
)

// buildFTSLiteralExpression converts Unicode token runs to FTS quoted literals.
// It never returns the input in an error.
func buildFTSLiteralExpression(input string) (string, error) {
	if len(input) > memory.MaxQueryBytes || !utf8.ValidString(input) {
		return "", fmt.Errorf("%w: lexical query bounds", memory.ErrInvalidRequest)
	}
	terms := make([]string, 0, 8)
	start := -1
	meaningful := false
	flush := func(end int) error {
		if start < 0 {
			return nil
		}
		term := input[start:end]
		start = -1
		if !meaningful {
			return nil
		}
		meaningful = false
		if len(term) > memory.MaxFTSTermBytes {
			return fmt.Errorf("%w: lexical term bounds", memory.ErrInvalidRequest)
		}
		if len(terms) >= memory.MaxFTSTerms {
			return fmt.Errorf("%w: lexical term count", memory.ErrInvalidRequest)
		}
		terms = append(terms, quoteFTSTerm(term))
		return nil
	}
	for offset, r := range input {
		if unicode.IsSpace(r) || unicode.IsControl(r) {
			if err := flush(offset); err != nil {
				return "", err
			}
			continue
		}
		if start < 0 {
			start = offset
		}
		if unicode.IsLetter(r) || unicode.IsMark(r) || unicode.IsNumber(r) || unicode.IsSymbol(r) {
			meaningful = true
		}
	}
	if err := flush(len(input)); err != nil {
		return "", err
	}
	return strings.Join(terms, " OR "), nil
}

func quoteFTSTerm(term string) string { return `"` + strings.ReplaceAll(term, `"`, `""`) + `"` }

func retrievalTextSafety(alias, column string, minimum, maximum int) string {
	name := alias + "." + column
	return fmt.Sprintf("typeof(%s)='text' AND length(CAST(%s AS BLOB)) BETWEEN %d AND %d", name, name, minimum, maximum)
}

func retrievalRecordSafety(alias string) string {
	bounded := func(column string, maximum int) string { return retrievalTextSafety(alias, column, 0, maximum) }
	jsonValue := func(column string, maximum int, kind string) string {
		name := alias + "." + column
		gate := bounded(column, maximum)
		projected := "CASE WHEN " + gate + " THEN " + name + " END"
		return gate + " AND json_valid(" + projected + ") AND json_type(" + projected + ")='" + kind + "'"
	}
	q := func(column string) string { return alias + "." + column }
	return strings.Join([]string{
		retrievalTextSafety(alias, "id", 1, memory.MaxIDBytes),
		retrievalTextSafety(alias, "scope_namespace", 1, memory.MaxNamespaceBytes),
		retrievalTextSafety(alias, "scope_id", 1, memory.MaxScopeIDBytes),
		retrievalTextSafety(alias, "kind", 1, memory.MaxKindBytes),
		bounded("semantic_key", memory.MaxSemanticKeyBytes),
		retrievalTextSafety(alias, "text_value", 1, memory.MaxRecordTextBytes),
		jsonValue("labels_json", maxLabelsJSONBytes, "array"),
		jsonValue("metadata_json", memory.MaxMetadataBytes, "object"),
		jsonValue("source_json", maxSourceJSONBytes, "object"),
		"typeof(" + q("confidence") + ") IN ('real','integer')",
		"typeof(" + q("revision") + ")='integer' AND " + q("revision") + " BETWEEN 1 AND " + strconv.FormatInt(math.MaxInt64, 10),
		retrievalTextSafety(alias, "created_at", len(timestampLayout), len(timestampLayout)),
		retrievalTextSafety(alias, "updated_at", len(timestampLayout), len(timestampLayout)),
		"(" + q("expires_at") + " IS NULL OR (" + retrievalTextSafety(alias, "expires_at", len(timestampLayout), len(timestampLayout)) + "))",
		"typeof(" + q("state") + ")='text' AND " + q("state") + "='active'",
		q("forgotten_at") + " IS NULL",
	}, " AND ")
}

func retrievalRecordProjection(alias string) string {
	safety := retrievalRecordSafety(alias)
	columns := []string{"id", "scope_namespace", "scope_id", "kind", "semantic_key", "text_value", "labels_json", "metadata_json", "source_json", "confidence", "revision", "created_at", "updated_at", "expires_at"}
	parts := make([]string, 0, len(columns)+1)
	parts = append(parts, "CASE WHEN "+safety+" THEN 1 ELSE 0 END")
	for _, column := range columns {
		parts = append(parts, "CASE WHEN "+safety+" THEN "+alias+"."+column+" END")
	}
	return strings.Join(parts, ",")
}

func retrievalFilter(request memory.RetrievalRequest, alias string, scopes []memory.Scope) ([]string, []any) {
	q := func(column string) string { return alias + "." + column }
	clauses := []string{q("state") + "=?"}
	arguments := []any{string(memory.RecordActive)}
	scopeClauses := make([]string, 0, len(scopes))
	for _, scope := range scopes {
		scopeClauses = append(scopeClauses, "("+q("scope_namespace")+"=? AND "+q("scope_id")+"=?)")
		arguments = append(arguments, scope.Namespace, scope.ID)
	}
	clauses = append(clauses, "("+strings.Join(scopeClauses, " OR ")+")")
	if len(request.Kinds) != 0 {
		clauses = append(clauses, q("kind")+" IN ("+placeholders(len(request.Kinds))+")")
		for _, kind := range request.Kinds {
			arguments = append(arguments, kind)
		}
	}
	if !request.IncludeExpired {
		expires := q("expires_at")
		safe := timestampPredicateSafety(expires) + " AND " + timestampPredicateSafety(q("created_at")) + " AND " + expires + ">=" + q("created_at")
		clauses = append(clauses, "("+expires+" IS NULL OR NOT ("+safe+") OR "+expires+">?)")
		arguments = append(arguments, formatTimestamp(request.Now))
	}
	return clauses, arguments
}

func buildBaselineQuery(request memory.RetrievalRequest, user memory.Scope) (string, []any) {
	clauses, arguments := retrievalFilter(request, "r", []memory.Scope{user})
	clauses = append(clauses, "r.semantic_key<>?", "r.kind IN (?,?)")
	arguments = append(arguments, "", "preference", "instruction", memory.MaxBaselineRecords)
	query := "SELECT " + retrievalRecordProjection("r") + " FROM memory_records r WHERE " + strings.Join(clauses, " AND ") + " ORDER BY r.updated_at DESC,r.id ASC LIMIT ?"
	return query, arguments
}

func buildFTSCandidateQuery(request memory.RetrievalRequest, expression string) (string, []any) {
	clauses, arguments := retrievalFilter(request, "r", request.Scopes)
	clauses = append(clauses, "memory_records_fts MATCH ?")
	arguments = append(arguments, expression, memory.MaxRetrievalCandidates)
	rank := "bm25(memory_records_fts,0.0,1.0,0.5,2.0,1.0)"
	ftsGate := func(column string, minimum, maximum int) string {
		qualified := "memory_records_fts." + column
		return "CASE WHEN typeof(" + qualified + ")='text' AND length(CAST(" + qualified + " AS BLOB)) BETWEEN " + strconv.Itoa(minimum) + " AND " + strconv.Itoa(maximum) + " THEN " + qualified + " END"
	}
	maxFTSLabelsBytes := memory.MaxLabels*memory.MaxLabelBytes + memory.MaxLabels - 1
	ftsProjection := strings.Join([]string{
		ftsGate("record_id", 1, memory.MaxIDBytes),
		ftsGate("text_value", 1, memory.MaxRecordTextBytes),
		ftsGate("kind", 1, memory.MaxKindBytes),
		ftsGate("semantic_key", 0, memory.MaxSemanticKeyBytes),
		ftsGate("labels", 0, maxFTSLabelsBytes),
	}, ",")
	query := "SELECT " + retrievalRecordProjection("r") + "," + ftsProjection + ",CASE WHEN typeof(" + rank + ") IN ('real','integer') THEN " + rank + " END " +
		"FROM memory_records_fts JOIN memory_records r ON memory_records_fts.record_id=r.id WHERE " + strings.Join(clauses, " AND ") +
		" ORDER BY " + rank + " ASC,r.updated_at DESC,r.id ASC LIMIT ?"
	return query, arguments
}

type retrievalKey struct{ kind, key string }

func buildWorkspaceReplacementQuery(request memory.RetrievalRequest, workspace memory.Scope, keys []retrievalKey) (string, []any) {
	values := make([]string, len(keys))
	arguments := make([]any, 0, len(keys)*2+len(request.Kinds)+4)
	for index, key := range keys {
		values[index] = "(?,?)"
		arguments = append(arguments, key.kind, key.key)
	}
	clauses, filterArguments := retrievalFilter(request, "r", []memory.Scope{workspace})
	arguments = append(arguments, filterArguments...)
	arguments = append(arguments, memory.MaxRetrievalCandidates)
	query := "WITH requested(kind,key) AS (VALUES " + strings.Join(values, ",") + ") SELECT " + retrievalRecordProjection("r") +
		" FROM memory_records r JOIN requested q ON q.kind=r.kind AND q.key=r.semantic_key WHERE " + strings.Join(clauses, " AND ") +
		" ORDER BY r.updated_at DESC,r.id ASC LIMIT ?"
	return query, arguments
}
