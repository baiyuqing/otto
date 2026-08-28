package session

import (
	"bytes"
	"encoding/json"
	"errors"
	"io"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestDecodePiV3LinearFixture(t *testing.T) {
	file, err := os.Open("testdata/pi-v3/linear.jsonl")
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()

	decoded, warnings, err := decodePiFile(file)
	if err != nil {
		t.Fatal(err)
	}
	if len(warnings) != 0 {
		t.Fatalf("warnings = %#v", warnings)
	}
	if decoded.Header.Type != "session" || decoded.Header.Version != 3 || decoded.Header.CWD != "/workspace" {
		t.Fatalf("header = %#v", decoded.Header)
	}
	if len(decoded.Entries) != 4 || decoded.Entries[0].ParentID != nil {
		t.Fatalf("entries = %#v", decoded.Entries)
	}
	if got := decoded.Entries[1].Message; got == nil || got.Role != "assistant" || len(got.ContentBlocks) != 2 || got.ContentBlocks[1].Type != "toolCall" {
		t.Fatalf("assistant message = %#v", got)
	}
	if got := decoded.Entries[2].Message; got == nil || got.Role != "toolResult" || got.ToolCallID != "call-1" || got.IsError == nil || *got.IsError {
		t.Fatalf("tool result message = %#v", got)
	}
}

func TestDecodePiV3TreeFixtureUsesExactEntryShapes(t *testing.T) {
	decoded := readPiFixture(t, "tree.jsonl")
	if decoded.Header.ParentSession == nil || *decoded.Header.ParentSession != "/workspace/parent.jsonl" {
		t.Fatalf("parentSession = %#v", decoded.Header.ParentSession)
	}

	var seen = make(map[string]*piEntry)
	for i := range decoded.Entries {
		seen[decoded.Entries[i].Type] = &decoded.Entries[i]
	}
	if seen["custom"] == nil || seen["custom"].Custom == nil || seen["custom"].Custom.CustomType != "otto.runtime" {
		t.Fatalf("custom entry = %#v", seen["custom"])
	}
	if seen["model_change"] == nil || seen["model_change"].ModelChange == nil || seen["model_change"].ModelChange.ModelID != "test-model-2" {
		t.Fatalf("model_change entry = %#v", seen["model_change"])
	}
	if seen["thinking_level_change"] == nil || seen["thinking_level_change"].ThinkingLevelChange == nil {
		t.Fatalf("thinking_level_change entry = %#v", seen["thinking_level_change"])
	}
	if seen["branch_summary"] == nil || seen["branch_summary"].BranchSummary == nil || seen["branch_summary"].BranchSummary.FromID != "a0000003" {
		t.Fatalf("branch_summary entry = %#v", seen["branch_summary"])
	}
	if seen["label"] == nil || seen["label"].Label == nil || seen["label"].Label.TargetID != "a0000002" {
		t.Fatalf("label entry = %#v", seen["label"])
	}
	if seen["session_info"] == nil || seen["session_info"].SessionInfo == nil || seen["session_info"].SessionInfo.Name == nil {
		t.Fatalf("session_info entry = %#v", seen["session_info"])
	}
}

func TestDecodePiV3CompactionFixture(t *testing.T) {
	decoded := readPiFixture(t, "compacted.jsonl")
	legacy := decoded.Entries[2].Compaction
	if legacy == nil || legacy.FirstKeptEntryID == nil || *legacy.FirstKeptEntryID != "c0000002" {
		t.Fatalf("legacy compaction = %#v", legacy)
	}
	checkpoint := decoded.Entries[3].Compaction
	if checkpoint == nil || checkpoint.Summary != "summary" || len(checkpoint.RetainedTail) != 1 || checkpoint.RetainedTail[0].Role != "user" {
		t.Fatalf("checkpoint compaction = %#v", checkpoint)
	}
	if checkpoint.Usage == nil || checkpoint.Usage.Input != 12 || checkpoint.Usage.Output != 4 || checkpoint.FromHook == nil || *checkpoint.FromHook {
		t.Fatalf("checkpoint metadata = %#v", checkpoint)
	}
}

func TestDecodePiV3CompactionBoundaryFixtures(t *testing.T) {
	retained := readPiFixture(t, "compaction-retained-tail-only.jsonl")
	checkpoint := retained.Entries[len(retained.Entries)-1].Compaction
	if checkpoint == nil || checkpoint.FirstKeptEntryID != nil || len(checkpoint.RetainedTail) != 1 || checkpoint.RetainedTail[0].Role != "user" {
		t.Fatalf("retained-tail-only checkpoint = %#v", checkpoint)
	}
	if !bytes.Contains(checkpoint.Details, []byte(`"readFiles":"malformed"`)) {
		t.Fatalf("malformed external details were not preserved: %s", checkpoint.Details)
	}

	dual := readPiFixture(t, "compaction-dual-form.jsonl")
	checkpoint = dual.Entries[3].Compaction
	if checkpoint == nil || checkpoint.FirstKeptEntryID == nil || *checkpoint.FirstKeptEntryID != "e0000003" || len(checkpoint.RetainedTail) != 1 {
		t.Fatalf("dual-form checkpoint = %#v", checkpoint)
	}
}

func TestDecodePiV3PreservesUnknownEntryRawJSON(t *testing.T) {
	decoded := readPiFixture(t, "unknown-entry.jsonl")
	entry := decoded.Entries[1]
	if entry.Type != "future_entry" || !bytes.Contains(entry.Raw, []byte(`"futureField"`)) {
		t.Fatalf("entry = %#v", entry)
	}

	encoded, err := encodePiRecord(entry)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(encoded, entry.Raw) {
		t.Fatalf("encoded unknown entry changed raw JSON:\n got: %s\nwant: %s", encoded, entry.Raw)
	}
}

func TestDecodePiV3PreservesUnknownFieldsOnSupportedObjects(t *testing.T) {
	decoded := readPiFixture(t, "unknown-entry.jsonl")
	if !bytes.Contains(decoded.Header.Raw, []byte(`"futureHeaderField"`)) {
		t.Fatalf("header raw JSON = %s", decoded.Header.Raw)
	}
	for _, index := range []int{0, 2} {
		entry := decoded.Entries[index]
		if !bytes.Contains(entry.Raw, []byte("future")) {
			t.Fatalf("supported entry raw JSON = %s", entry.Raw)
		}
		encoded, err := encodePiRecord(entry)
		if err != nil {
			t.Fatal(err)
		}
		if !bytes.Equal(encoded, entry.Raw) {
			t.Fatalf("supported entry changed raw JSON:\n got: %s\nwant: %s", encoded, entry.Raw)
		}
	}

	encodedHeader, err := encodePiRecord(decoded.Header)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(encodedHeader, decoded.Header.Raw) {
		t.Fatalf("supported header changed raw JSON:\n got: %s\nwant: %s", encodedHeader, decoded.Header.Raw)
	}
}

func TestDecodePiV3ToleratesAdditionalNestedFields(t *testing.T) {
	const input = `{"type":"message","id":"e0000001","parentId":null,"timestamp":"2026-08-27T12:00:00Z","message":{"role":"assistant","content":[{"type":"text","text":"ok","futureText":true}],"api":"openai-completions","provider":"openai-compatible","model":"test-model","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0,"futureCost":0},"futureUsage":0},"stopReason":"stop","timestamp":1787832000000,"futureMessage":true},"futureEntry":true}`
	entry, err := decodePiEntry([]byte(input))
	if err != nil {
		t.Fatal(err)
	}
	if entry.Message == nil || !bytes.Equal(entry.Raw, []byte(input)) {
		t.Fatalf("entry = %#v", entry)
	}
}

func TestDecodePiV3AcceptsFinalRecordWithoutLF(t *testing.T) {
	contents, err := os.ReadFile("testdata/pi-v3/linear.jsonl")
	if err != nil {
		t.Fatal(err)
	}
	contents = bytes.TrimSuffix(contents, []byte{'\n'})
	decoded, warnings, err := decodePiFile(bytes.NewReader(contents))
	if err != nil {
		t.Fatal(err)
	}
	if len(warnings) != 0 || len(decoded.Entries) != 4 {
		t.Fatalf("decoded = %#v, warnings = %#v", decoded, warnings)
	}
}

func TestDecodePiV3RejectsMalformedKnownShapes(t *testing.T) {
	validBase := `"id":"e0000001","parentId":null,"timestamp":"2026-08-27T12:00:00Z"`
	tests := map[string]string{
		"missing base parentId": `{"type":"future_entry","id":"e0000001","timestamp":"2026-08-27T12:00:00Z"}`,
		"wrong base id":         `{"type":"future_entry","id":1,"parentId":null,"timestamp":"2026-08-27T12:00:00Z"}`,
		"custom type":           `{"type":"custom",` + validBase + `,"customType":1}`,
		"model id":              `{"type":"model_change",` + validBase + `,"provider":"openai-compatible"}`,
		"thinking level":        `{"type":"thinking_level_change",` + validBase + `,"thinkingLevel":false}`,
		"message role":          `{"type":"message",` + validBase + `,"message":{"role":1,"timestamp":1}}`,
		"unknown message role":  `{"type":"message",` + validBase + `,"message":{"role":"futureRole","timestamp":1}}`,
		"user content":          `{"type":"message",` + validBase + `,"message":{"role":"user","content":42,"timestamp":1}}`,
		"unknown content type":  `{"type":"message",` + validBase + `,"message":{"role":"user","content":[{"type":"futureBlock"}],"timestamp":1}}`,
		"text block":            `{"type":"message",` + validBase + `,"message":{"role":"user","content":[{"type":"text","text":false}],"timestamp":1}}`,
		"text signature":        `{"type":"message",` + validBase + `,"message":{"role":"user","content":[{"type":"text","text":"ok","textSignature":1}],"timestamp":1}}`,
		"tool arguments":        `{"type":"message",` + validBase + `,"message":{"role":"assistant","content":[{"type":"toolCall","id":"call-1","name":"read","arguments":[]}],"api":"openai-completions","provider":"openai-compatible","model":"test","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"toolUse","timestamp":1}}`,
		"tool namespace":        `{"type":"message",` + validBase + `,"message":{"role":"assistant","content":[{"type":"toolCall","id":"call-1","name":"read","arguments":{},"namespace":1}],"api":"openai-completions","provider":"openai-compatible","model":"test","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"toolUse","timestamp":1}}`,
		"usage":                 `{"type":"message",` + validBase + `,"message":{"role":"assistant","content":[],"api":"openai-completions","provider":"openai-compatible","model":"test","usage":{"input":"zero"},"stopReason":"stop","timestamp":1}}`,
		"deferred handle":       `{"type":"message",` + validBase + `,"message":{"role":"assistant","content":[],"api":"openai-completions","provider":"openai-compatible","model":"test","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"deferred","deferred":{"provider":1},"timestamp":1}}`,
		"diagnostic timestamp":  `{"type":"message",` + validBase + `,"message":{"role":"assistant","content":[],"api":"openai-completions","provider":"openai-compatible","model":"test","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"stop","diagnostics":[{"type":"warning","timestamp":"now"}],"timestamp":1}}`,
		"tool result flag":      `{"type":"message",` + validBase + `,"message":{"role":"toolResult","toolCallId":"call-1","toolName":"read","content":[],"isError":"false","timestamp":1}}`,
		"null bash exit code":   `{"type":"message",` + validBase + `,"message":{"role":"bashExecution","command":"pwd","output":"/workspace","exitCode":null,"cancelled":false,"truncated":false,"timestamp":1}}`,
		"compaction boundary":   `{"type":"compaction",` + validBase + `,"summary":"summary","tokensBefore":1}`,
		"compaction tail":       `{"type":"compaction",` + validBase + `,"summary":"summary","tokensBefore":1,"retainedTail":{}}`,
		"branch from id":        `{"type":"branch_summary",` + validBase + `,"fromId":1,"summary":"summary"}`,
		"custom display":        `{"type":"custom_message",` + validBase + `,"customType":"fixture","content":"text","display":"true"}`,
		"label target":          `{"type":"label",` + validBase + `,"targetId":null}`,
		"session name":          `{"type":"session_info",` + validBase + `,"name":1}`,
	}

	for name, input := range tests {
		t.Run(name, func(t *testing.T) {
			_, err := decodePiEntry([]byte(input))
			if !errors.Is(err, ErrInvalidSession) {
				t.Fatalf("error = %v, want ErrInvalidSession", err)
			}
		})
	}
}

func TestDecodePiV3RejectsUnsupportedHeaders(t *testing.T) {
	for name, input := range map[string]string{
		"old Otto":   `{"type":"header","header":{"version":1}}`,
		"old Pi":     `{"type":"session","version":2,"id":"id","timestamp":"now","cwd":"/workspace"}`,
		"future Pi":  `{"type":"session","version":4,"id":"id","timestamp":"now","cwd":"/workspace"}`,
		"missing v3": `{"type":"session","id":"id","timestamp":"now","cwd":"/workspace"}`,
	} {
		t.Run(name, func(t *testing.T) {
			_, err := decodePiHeader([]byte(input))
			if !errors.Is(err, ErrUnsupportedSessionFormat) {
				t.Fatalf("error = %v, want ErrUnsupportedSessionFormat", err)
			}
		})
	}
}

func TestDecodePiV3RejectsMalformedHeaders(t *testing.T) {
	for name, input := range map[string]string{
		"not JSON":        `not-json`,
		"null version":    `{"type":"session","version":null,"id":"id","timestamp":"now","cwd":"/workspace"}`,
		"missing id":      `{"type":"session","version":3,"timestamp":"now","cwd":"/workspace"}`,
		"wrong timestamp": `{"type":"session","version":3,"id":"id","timestamp":1,"cwd":"/workspace"}`,
		"parent session":  `{"type":"session","version":3,"id":"id","timestamp":"now","cwd":"/workspace","parentSession":false}`,
		"trailing JSON":   `{"type":"session","version":3,"id":"id","timestamp":"now","cwd":"/workspace"}{}`,
		"top-level array": `[]`,
	} {
		t.Run(name, func(t *testing.T) {
			_, err := decodePiHeader([]byte(input))
			if !errors.Is(err, ErrInvalidSession) {
				t.Fatalf("error = %v, want ErrInvalidSession", err)
			}
		})
	}
}

func TestDecodePiV3RejectsOversizedEntry(t *testing.T) {
	_, _, err := decodePiFile(strings.NewReader(strings.Repeat("x", maxSessionEntryBytes+1)))
	if !errors.Is(err, ErrSessionEntryTooLarge) {
		t.Fatalf("error = %v", err)
	}
}

func TestDecodePiV3RejectsOversizedFile(t *testing.T) {
	reader := &lengthOnlyReader{length: maxSessionFileBytes + 1}
	_, _, err := decodePiFile(reader)
	if !errors.Is(err, ErrSessionFileTooLarge) {
		t.Fatalf("error = %v", err)
	}
	if reader.read {
		t.Fatal("reader with an oversized declared length was read")
	}
}

func TestDecodePiV3FixturesAreExactLFDelimitedJSON(t *testing.T) {
	files, err := filepath.Glob("testdata/pi-v3/*.jsonl")
	if err != nil {
		t.Fatal(err)
	}
	if len(files) != 6 {
		t.Fatalf("fixture count = %d, want 6", len(files))
	}
	for _, path := range files {
		t.Run(filepath.Base(path), func(t *testing.T) {
			contents, err := os.ReadFile(path)
			if err != nil {
				t.Fatal(err)
			}
			if !bytes.HasSuffix(contents, []byte{'\n'}) || bytes.Contains(contents, []byte{'\r'}) {
				t.Fatalf("fixture is not exact LF-delimited JSONL: %q", contents)
			}
			for lineNumber, line := range bytes.Split(bytes.TrimSuffix(contents, []byte{'\n'}), []byte{'\n'}) {
				if !json.Valid(line) {
					t.Fatalf("line %d is not JSON: %s", lineNumber+1, line)
				}
			}
		})
	}
}

func TestEncodePiRecordRejectsOversizedRawRecordBeforeDecoding(t *testing.T) {
	entry := piEntry{Raw: json.RawMessage(strings.Repeat(" ", maxSessionEntryBytes+1))}
	_, err := encodePiRecord(entry)
	if !errors.Is(err, ErrSessionEntryTooLarge) {
		t.Fatalf("error = %v, want ErrSessionEntryTooLarge", err)
	}
}

func TestEncodePiRecordReturnsErrorForInvalidTypedPayload(t *testing.T) {
	_, err := encodePiRecord(piEntry{
		piEntryBase: piEntryBase{Type: "message", ID: "e0000001", Timestamp: "2026-08-27T12:00:01Z"},
		Message:     &piMessage{Role: "user", Content: json.RawMessage(`{"unterminated"`), Timestamp: 1},
	})
	if err == nil {
		t.Fatal("expected invalid typed payload to fail")
	}
}

func TestEncodePiRecordUsesExactFieldNames(t *testing.T) {
	header, err := encodePiRecord(piHeader{
		Type:      "session",
		Version:   PiSessionVersion,
		ID:        "950e8400-e29b-41d4-a716-446655440000",
		Timestamp: "2026-08-27T12:00:00Z",
		CWD:       "/workspace",
	})
	if err != nil {
		t.Fatal(err)
	}
	assertRawJSONFields(t, header, "type", "version", "id", "timestamp", "cwd")

	entry, err := encodePiRecord(piEntry{
		piEntryBase: piEntryBase{Type: "message", ID: "e0000001", ParentID: nil, Timestamp: "2026-08-27T12:00:01Z"},
		Message:     &piMessage{Role: "user", Content: json.RawMessage(`"hello"`), Timestamp: 1787832001000},
	})
	if err != nil {
		t.Fatal(err)
	}
	assertRawJSONFields(t, entry, "type", "id", "parentId", "timestamp", "message")
	if bytes.Contains(entry, []byte("parent_id")) {
		t.Fatalf("entry used non-Pi casing: %s", entry)
	}
}

func readPiFixture(t *testing.T, name string) piFile {
	t.Helper()
	file, err := os.Open(filepath.Join("testdata/pi-v3", name))
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	decoded, warnings, err := decodePiFile(file)
	if err != nil {
		t.Fatal(err)
	}
	if len(warnings) != 0 {
		t.Fatalf("warnings = %#v", warnings)
	}
	return decoded
}

func assertRawJSONFields(t *testing.T, raw []byte, want ...string) {
	t.Helper()
	var object map[string]json.RawMessage
	if err := json.Unmarshal(raw, &object); err != nil {
		t.Fatal(err)
	}
	if len(object) != len(want) {
		t.Fatalf("fields = %v, want %v; JSON = %s", object, want, raw)
	}
	for _, field := range want {
		if _, ok := object[field]; !ok {
			t.Fatalf("field %q missing from %s", field, raw)
		}
	}
}

type lengthOnlyReader struct {
	length int
	read   bool
}

func (reader *lengthOnlyReader) Len() int {
	return reader.length
}

func (reader *lengthOnlyReader) Read([]byte) (int, error) {
	reader.read = true
	return 0, io.EOF
}
