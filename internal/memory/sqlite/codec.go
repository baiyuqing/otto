package sqlite

import (
	"bytes"
	"encoding/json"
	"errors"
	"math"
	"sort"
	"time"

	"github.com/baiyuqing/otto/internal/memory"
)

const (
	timestampLayout    = "2006-01-02T15:04:05.000000000Z"
	maxLabelsJSONBytes = 8192
	maxSourceJSONBytes = 8192
)

type provenanceJSON struct {
	Origin         memory.Origin `json:"origin"`
	SessionID      string        `json:"session_id"`
	MessageIDs     []string      `json:"message_ids"`
	ObservationID  string        `json:"observation_id"`
	DecisionAt     *string       `json:"decision_at"`
	DecisionSource memory.Origin `json:"decision_source"`
}

type encodedRecord struct {
	labels, metadata, source []byte
	created, updated         string
	expires                  *string
}

func encodeRecord(record memory.Record) (encodedRecord, error) {
	labels := record.Labels
	if labels == nil {
		labels = []string{}
	}
	metadata := record.Metadata
	if metadata == nil {
		metadata = map[string]string{}
	}
	labelsJSON, err := json.Marshal(labels)
	if err != nil || len(labelsJSON) > maxLabelsJSONBytes {
		return encodedRecord{}, memory.ErrCorrupt
	}
	metadataJSON, err := json.Marshal(metadata)
	if err != nil || len(metadataJSON) > memory.MaxMetadataBytes {
		return encodedRecord{}, memory.ErrCorrupt
	}
	wire := provenanceJSON{
		Origin: record.Source.Origin, SessionID: record.Source.SessionID,
		MessageIDs: record.Source.MessageIDs, ObservationID: record.Source.ObservationID,
		DecisionSource: record.Source.DecisionSource,
	}
	if wire.MessageIDs == nil {
		wire.MessageIDs = []string{}
	}
	if record.Source.DecisionAt != nil {
		value := formatTimestamp(*record.Source.DecisionAt)
		wire.DecisionAt = &value
	}
	sourceJSON, err := json.Marshal(wire)
	if err != nil || len(sourceJSON) > maxSourceJSONBytes {
		return encodedRecord{}, memory.ErrCorrupt
	}
	var expires *string
	if record.ExpiresAt != nil {
		value := formatTimestamp(*record.ExpiresAt)
		expires = &value
	}
	return encodedRecord{
		labels: labelsJSON, metadata: metadataJSON, source: sourceJSON,
		created: formatTimestamp(record.CreatedAt), updated: formatTimestamp(record.UpdatedAt), expires: expires,
	}, nil
}

func decodeLabels(raw []byte) ([]string, error) {
	if len(raw) > maxLabelsJSONBytes {
		return nil, memory.ErrCorrupt
	}
	var value []string
	if err := strictJSON(raw, &value); err != nil || value == nil {
		return nil, memory.ErrCorrupt
	}
	canonical, err := json.Marshal(value)
	if err != nil || !bytes.Equal(raw, canonical) {
		return nil, memory.ErrCorrupt
	}
	return value, nil
}

func decodeMetadata(raw []byte) (map[string]string, error) {
	if len(raw) > memory.MaxMetadataBytes {
		return nil, memory.ErrCorrupt
	}
	var value map[string]string
	if err := strictJSON(raw, &value); err != nil || value == nil {
		return nil, memory.ErrCorrupt
	}
	canonical, err := json.Marshal(value)
	if err != nil || !bytes.Equal(raw, canonical) {
		return nil, memory.ErrCorrupt
	}
	return value, nil
}

func decodeProvenance(raw []byte) (memory.Provenance, error) {
	if len(raw) > maxSourceJSONBytes {
		return memory.Provenance{}, memory.ErrCorrupt
	}
	var wire provenanceJSON
	if err := strictJSON(raw, &wire); err != nil || wire.MessageIDs == nil {
		return memory.Provenance{}, memory.ErrCorrupt
	}
	canonical, err := json.Marshal(wire)
	if err != nil || !bytes.Equal(raw, canonical) {
		return memory.Provenance{}, memory.ErrCorrupt
	}
	messages := make([]string, len(wire.MessageIDs))
	copy(messages, wire.MessageIDs)
	value := memory.Provenance{
		Origin: wire.Origin, SessionID: wire.SessionID, MessageIDs: messages,
		ObservationID: wire.ObservationID, DecisionSource: wire.DecisionSource,
	}
	if wire.DecisionAt != nil {
		parsed, err := parseTimestamp(*wire.DecisionAt)
		if err != nil {
			return memory.Provenance{}, err
		}
		value.DecisionAt = &parsed
	}
	return value, nil
}

func strictJSON(raw []byte, destination any) error {
	decoder := json.NewDecoder(bytes.NewReader(raw))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(destination); err != nil {
		return err
	}
	if decoder.More() {
		return errors.New("trailing JSON")
	}
	var extra any
	if err := decoder.Decode(&extra); err == nil {
		return errors.New("trailing JSON value")
	}
	return nil
}

func formatTimestamp(value time.Time) string { return value.UTC().Format(timestampLayout) }

func parseTimestamp(value string) (time.Time, error) {
	if len(value) != len(timestampLayout) {
		return time.Time{}, memory.ErrCorrupt
	}
	parsed, err := time.Parse(timestampLayout, value)
	if err != nil || formatTimestamp(parsed) != value || parsed.Location() != time.UTC {
		return time.Time{}, memory.ErrCorrupt
	}
	return parsed, nil
}

func ftsLabels(labels []string) string {
	copyLabels := append([]string(nil), labels...)
	sort.Strings(copyLabels)
	var size int
	for _, label := range copyLabels {
		size += len(label)
	}
	if len(copyLabels) > 1 {
		size += len(copyLabels) - 1
	}
	buffer := make([]byte, 0, size)
	for index, label := range copyLabels {
		if index != 0 {
			buffer = append(buffer, '\n')
		}
		buffer = append(buffer, label...)
	}
	return string(buffer)
}

func validStoredFloat(value float64) bool {
	return !math.IsNaN(value) && !math.IsInf(value, 0) && value >= 0 && value <= 1
}
