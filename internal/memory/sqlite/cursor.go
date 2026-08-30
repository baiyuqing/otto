package sqlite

import (
	"bytes"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"sort"
	"strconv"

	"github.com/baiyuqing/otto/internal/memory"
)

const (
	recordCursorVersion   = 1
	maxDecodedCursorBytes = 3 * 1024
)

type recordCursor struct {
	Version     int    `json:"v"`
	Fingerprint string `json:"fingerprint"`
	Generation  string `json:"generation"`
	UpdatedAt   string `json:"updated_at"`
	ID          string `json:"id"`
}

type listFingerprint struct {
	Domain         string                  `json:"domain"`
	Scopes         []memory.Scope          `json:"scopes"`
	Kinds          []string                `json:"kinds"`
	Labels         []string                `json:"labels"`
	States         []memory.CandidateState `json:"states,omitempty"`
	IncludeExpired bool                    `json:"include_expired"`
	Now            string                  `json:"now,omitempty"`
}

func fingerprintList(request memory.ListRequest) (string, error) {
	canonical := listFingerprint{
		Domain: "records", Scopes: append([]memory.Scope(nil), request.Scopes...), Kinds: append([]string(nil), request.Kinds...),
		Labels: append([]string(nil), request.Labels...), IncludeExpired: request.IncludeExpired,
	}
	sort.Slice(canonical.Scopes, func(i, j int) bool {
		if canonical.Scopes[i].Namespace != canonical.Scopes[j].Namespace {
			return canonical.Scopes[i].Namespace < canonical.Scopes[j].Namespace
		}
		return canonical.Scopes[i].ID < canonical.Scopes[j].ID
	})
	sort.Strings(canonical.Kinds)
	sort.Strings(canonical.Labels)
	if !request.IncludeExpired {
		canonical.Now = formatTimestamp(request.Now)
	}
	encoded, err := json.Marshal(canonical)
	if err != nil {
		return "", memory.ErrInvalidCursor
	}
	digest := sha256.Sum256(encoded)
	return hex.EncodeToString(digest[:]), nil
}

func fingerprintCandidates(request memory.CandidateListRequest) (string, error) {
	canonical := listFingerprint{
		Domain: "candidates", Scopes: append([]memory.Scope(nil), request.Scopes...),
		States: append([]memory.CandidateState(nil), request.States...),
	}
	sort.Slice(canonical.Scopes, func(i, j int) bool {
		if canonical.Scopes[i].Namespace != canonical.Scopes[j].Namespace {
			return canonical.Scopes[i].Namespace < canonical.Scopes[j].Namespace
		}
		return canonical.Scopes[i].ID < canonical.Scopes[j].ID
	})
	sort.Slice(canonical.States, func(i, j int) bool { return canonical.States[i] < canonical.States[j] })
	encoded, err := json.Marshal(canonical)
	if err != nil {
		return "", memory.ErrInvalidCursor
	}
	digest := sha256.Sum256(encoded)
	return hex.EncodeToString(digest[:]), nil
}

func fingerprintTombstones(request memory.TombstoneListRequest) (string, error) {
	canonical := listFingerprint{Domain: "tombstones", Scopes: append([]memory.Scope(nil), request.Scopes...)}
	sort.Slice(canonical.Scopes, func(i, j int) bool {
		if canonical.Scopes[i].Namespace != canonical.Scopes[j].Namespace {
			return canonical.Scopes[i].Namespace < canonical.Scopes[j].Namespace
		}
		return canonical.Scopes[i].ID < canonical.Scopes[j].ID
	})
	encoded, err := json.Marshal(canonical)
	if err != nil {
		return "", memory.ErrInvalidCursor
	}
	digest := sha256.Sum256(encoded)
	return hex.EncodeToString(digest[:]), nil
}

func encodeRecordCursor(fingerprint string, generation uint64, updatedAt, id string) (string, error) {
	payload := recordCursor{
		Version: recordCursorVersion, Fingerprint: fingerprint,
		Generation: strconv.FormatUint(generation, 10), UpdatedAt: updatedAt, ID: id,
	}
	raw, err := json.Marshal(payload)
	if err != nil || len(raw) > maxDecodedCursorBytes {
		return "", memory.ErrInvalidCursor
	}
	encodedLength := base64.RawURLEncoding.EncodedLen(len(raw))
	if encodedLength > memory.MaxCursorBytes {
		return "", memory.ErrInvalidCursor
	}
	output := make([]byte, encodedLength)
	base64.RawURLEncoding.Encode(output, raw)
	return string(output), nil
}

func decodeRecordCursor(value, fingerprint string) (recordCursor, uint64, error) {
	if value == "" {
		return recordCursor{}, 0, nil
	}
	if len(value) > memory.MaxCursorBytes {
		return recordCursor{}, 0, memory.ErrInvalidCursor
	}
	decodedLength := base64.RawURLEncoding.DecodedLen(len(value))
	if decodedLength > maxDecodedCursorBytes {
		return recordCursor{}, 0, memory.ErrInvalidCursor
	}
	raw := make([]byte, decodedLength)
	written, err := base64.RawURLEncoding.Decode(raw, []byte(value))
	if err != nil || written > maxDecodedCursorBytes {
		return recordCursor{}, 0, memory.ErrInvalidCursor
	}
	raw = raw[:written]
	var payload recordCursor
	decoder := json.NewDecoder(bytes.NewReader(raw))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(&payload); err != nil {
		return recordCursor{}, 0, memory.ErrInvalidCursor
	}
	var extra any
	if err := decoder.Decode(&extra); err == nil {
		return recordCursor{}, 0, memory.ErrInvalidCursor
	}
	canonical, err := json.Marshal(payload)
	if err != nil || !bytes.Equal(canonical, raw) || payload.Version != recordCursorVersion || payload.Fingerprint != fingerprint {
		return recordCursor{}, 0, memory.ErrInvalidCursor
	}
	generation, err := strconv.ParseUint(payload.Generation, 10, 64)
	if err != nil || strconv.FormatUint(generation, 10) != payload.Generation {
		return recordCursor{}, 0, memory.ErrInvalidCursor
	}
	if _, err := parseTimestamp(payload.UpdatedAt); err != nil || !validCursorID(payload.ID) {
		return recordCursor{}, 0, memory.ErrInvalidCursor
	}
	return payload, generation, nil
}

func validCursorID(value string) bool {
	if len(value) == 0 || len(value) > memory.MaxIDBytes {
		return false
	}
	for index := range len(value) {
		c := value[index]
		if c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' {
			continue
		}
		switch c {
		case '.', '_', ':', '-':
			continue
		default:
			return false
		}
	}
	return true
}
