package session

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"reflect"
	"regexp"
	"strings"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/model"
)

func TestCreateWritesPiV3HeaderAndOttoRuntimeEntry(t *testing.T) {
	header := testHeader(t)
	store, err := Create(t.TempDir(), header)
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()

	lines := readJSONLines(t, store.Path())
	if len(lines) != 2 {
		t.Fatalf("line count = %d, want 2", len(lines))
	}
	assertJSONEqual(t, lines[0], map[string]any{
		"type":      "session",
		"version":   float64(3),
		"id":        header.ID,
		"timestamp": header.CreatedAt.Format(time.RFC3339Nano),
		"cwd":       header.Workspace,
	})

	var runtimeEntry struct {
		Type       string          `json:"type"`
		ID         string          `json:"id"`
		ParentID   *string         `json:"parentId"`
		Timestamp  string          `json:"timestamp"`
		CustomType string          `json:"customType"`
		Data       RuntimeMetadata `json:"data"`
	}
	if err := json.Unmarshal(lines[1], &runtimeEntry); err != nil {
		t.Fatal(err)
	}
	if runtimeEntry.Type != "custom" || !validTestEntryID(runtimeEntry.ID) || runtimeEntry.ParentID != nil ||
		runtimeEntry.Timestamp != header.CreatedAt.Format(time.RFC3339Nano) || runtimeEntry.CustomType != "otto.runtime" {
		t.Fatalf("runtime entry = %#v", runtimeEntry)
	}
	if want := (RuntimeMetadata{Profile: header.Profile, Provider: header.Provider, Model: header.Model}); runtimeEntry.Data != want {
		t.Fatalf("runtime metadata = %#v, want %#v", runtimeEntry.Data, want)
	}

	contents, err := os.ReadFile(store.Path())
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.HasSuffix(contents, []byte{'\n'}) || bytes.Contains(contents, []byte{'\r'}) {
		t.Fatalf("session is not LF-delimited: %q", contents)
	}
}

func TestOpenRejectsOldOttoV1WithoutMutation(t *testing.T) {
	path := writeFixture(t, `{"type":"header","header":{"version":1}}`+"\n")
	before := readFile(t, path)

	if _, _, err := Open(path); !errors.Is(err, ErrUnsupportedSessionFormat) {
		t.Fatalf("Open() error = %v, want ErrUnsupportedSessionFormat", err)
	}
	if after := readFile(t, path); !bytes.Equal(after, before) {
		t.Fatal("old file mutated by Open")
	}
	if _, err := ReadHeader(path); !errors.Is(err, ErrUnsupportedSessionFormat) {
		t.Fatalf("ReadHeader() error = %v, want ErrUnsupportedSessionFormat", err)
	}
	if after := readFile(t, path); !bytes.Equal(after, before) {
		t.Fatal("old file mutated by ReadHeader")
	}
}

func TestStoreRoundTripsPiMessagesAndParentChain(t *testing.T) {
	header := testHeader(t)
	store, err := Create(t.TempDir(), header)
	if err != nil {
		t.Fatal(err)
	}
	messages := []model.Message{
		{ID: "domain-user", Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "hello"}}, CreatedAt: time.Unix(2, 123456789).UTC()},
		{ID: "domain-assistant", Role: model.RoleAssistant, Blocks: []model.Block{
			{Type: model.BlockText, Text: "reading"},
			{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "read", Arguments: json.RawMessage(`{"path":"README.md"}`)},
		}, CreatedAt: time.Unix(3, 0).UTC(), FinishReason: model.FinishToolCalls, Usage: &model.Usage{InputTokens: 7, OutputTokens: 2}},
		{ID: "domain-tool", Role: model.RoleTool, Blocks: []model.Block{{Type: model.BlockToolResult, Text: "contents", ToolCallID: "call-1", ToolName: "read"}}, CreatedAt: time.Unix(4, 0).UTC()},
		{ID: "domain-final", Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "done"}}, CreatedAt: time.Unix(5, 0).UTC(), FinishReason: model.FinishStop},
	}
	for _, message := range messages {
		if err := store.Append(context.Background(), message); err != nil {
			t.Fatal(err)
		}
	}
	persistedMessages := store.Messages()
	if len(persistedMessages) != len(messages) {
		t.Fatalf("Messages() = %#v", persistedMessages)
	}
	for i := range persistedMessages {
		if !validTestEntryID(persistedMessages[i].ID) {
			t.Fatalf("message %d ID = %q, want generated entry ID", i, persistedMessages[i].ID)
		}
		messages[i].ID = persistedMessages[i].ID
	}

	path := store.Path()
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	reopened, warnings, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer reopened.Close()
	if len(warnings) != 0 || !reflect.DeepEqual(reopened.Messages(), messages) {
		t.Fatalf("unexpected reopen result: warnings=%v messages=%#v want=%#v", warnings, reopened.Messages(), messages)
	}
	if reopened.Header() != header {
		t.Fatalf("Header() = %#v, want %#v", reopened.Header(), header)
	}

	decoded := decodeFixture(t, path)
	if len(decoded.Entries) != 5 {
		t.Fatalf("entry count = %d, want 5", len(decoded.Entries))
	}
	for i, entry := range decoded.Entries {
		if !validTestEntryID(entry.ID) {
			t.Fatalf("entry %d ID = %q", i, entry.ID)
		}
		if i == 0 {
			if entry.ParentID != nil {
				t.Fatalf("first parent = %v, want nil", entry.ParentID)
			}
		} else if entry.ParentID == nil || *entry.ParentID != decoded.Entries[i-1].ID {
			t.Fatalf("entry %d parent = %v, want %q", i, entry.ParentID, decoded.Entries[i-1].ID)
		}
	}
	if got := decoded.Entries[2].Message; got == nil || got.Role != "assistant" || got.StopReason != "toolUse" || got.Provider != header.Provider || got.Model != header.Model || got.Usage == nil || got.Usage.Input != 7 || got.Usage.Output != 2 {
		t.Fatalf("assistant Pi message = %#v", got)
	}
	if got := decoded.Entries[3].Message; got == nil || got.Role != "toolResult" || got.ToolCallID != "call-1" || got.IsError == nil || *got.IsError {
		t.Fatalf("tool-result Pi message = %#v", got)
	}
}

func TestOpenReadsSupportedExternalPiMessages(t *testing.T) {
	path := filepath.Join("testdata", "pi-v3", "linear.jsonl")
	store, warnings, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	if len(warnings) != 0 {
		t.Fatalf("warnings = %#v", warnings)
	}
	messages := store.Messages()
	if got, want := len(messages), 4; got != want {
		t.Fatalf("message count = %d, want %d", got, want)
	}
	if messages[0].Role != model.RoleUser || messages[0].Text() != "run the check" ||
		messages[1].Role != model.RoleAssistant || messages[1].FinishReason != model.FinishToolCalls ||
		messages[2].Role != model.RoleTool || messages[2].Blocks[0].ToolCallID != "call-1" ||
		messages[3].FinishReason != model.FinishStop {
		t.Fatalf("messages = %#v", messages)
	}
}

func TestReadHeaderDerivesOttoRuntimeMetadata(t *testing.T) {
	header := testHeader(t)
	store, err := Create(t.TempDir(), header)
	if err != nil {
		t.Fatal(err)
	}
	path := store.Path()
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}

	got, err := ReadHeader(path)
	if err != nil {
		t.Fatal(err)
	}
	if got != header {
		t.Fatalf("ReadHeader() = %#v, want %#v", got, header)
	}
}

func TestReadHeaderLeavesIncompleteTailForOpenRecovery(t *testing.T) {
	header := testHeader(t)
	store, err := Create(t.TempDir(), header)
	if err != nil {
		t.Fatal(err)
	}
	if err := store.Append(context.Background(), model.Message{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "hello"}}, CreatedAt: time.Unix(2, 0).UTC()}); err != nil {
		t.Fatal(err)
	}
	path := store.Path()
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	appendFixture(t, path, `{"type":"message","id":"12345678","parentId":`)
	before := readFile(t, path)

	got, err := ReadHeader(path)
	if err != nil {
		t.Fatal(err)
	}
	if got != header {
		t.Fatalf("ReadHeader() = %#v, want %#v", got, header)
	}
	if after := readFile(t, path); !bytes.Equal(after, before) {
		t.Fatal("ReadHeader mutated incomplete tail")
	}

	reopened, warnings, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer reopened.Close()
	if len(warnings) != 1 {
		t.Fatalf("Open() warnings = %#v, want recovery warning", warnings)
	}
}

func TestCreateRejectsTimestampOutsideRFC3339NanoRange(t *testing.T) {
	root := t.TempDir()
	header := testHeader(t)
	header.CreatedAt = time.Date(10000, time.January, 1, 0, 0, 0, 0, time.UTC)

	store, err := Create(root, header)
	if err == nil {
		_ = store.Close()
		t.Fatal("Create() succeeded with an out-of-range timestamp")
	}
	if !errors.Is(err, ErrInvalidSession) {
		t.Fatalf("Create() error = %v, want ErrInvalidSession", err)
	}
	entries, readErr := os.ReadDir(root)
	if readErr != nil {
		t.Fatal(readErr)
	}
	if len(entries) != 0 {
		t.Fatalf("Create() wrote session state before rejecting timestamp: %#v", entries)
	}
}

func TestCreateUsesExpectedPermissionsAndPath(t *testing.T) {
	root := t.TempDir()
	workspaceRoot := t.TempDir()
	canonicalWorkspace := filepath.Join(workspaceRoot, "project")
	if err := os.Mkdir(canonicalWorkspace, 0o700); err != nil {
		t.Fatal(err)
	}
	symlinkWorkspace := filepath.Join(workspaceRoot, "workspace-link")
	if err := os.Symlink(canonicalWorkspace, symlinkWorkspace); err != nil {
		t.Fatal(err)
	}

	header := testHeader(t)
	header.Workspace = symlinkWorkspace
	store, err := Create(root, header)
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()

	resolvedWorkspace, err := filepath.EvalSymlinks(canonicalWorkspace)
	if err != nil {
		t.Fatal(err)
	}
	sum := sha256.Sum256([]byte(resolvedWorkspace))
	workspaceKey := hex.EncodeToString(sum[:])[:16]
	wantPath := filepath.Join(root, workspaceKey, header.ID+".jsonl")
	if got := store.Path(); got != wantPath {
		t.Fatalf("Path() = %q, want %q", got, wantPath)
	}

	dirInfo, err := os.Stat(filepath.Dir(wantPath))
	if err != nil {
		t.Fatal(err)
	}
	if got := dirInfo.Mode().Perm(); got != 0o700 {
		t.Fatalf("directory mode = %o, want %o", got, 0o700)
	}
	fileInfo, err := os.Stat(wantPath)
	if err != nil {
		t.Fatal(err)
	}
	if got := fileInfo.Mode().Perm(); got != 0o600 {
		t.Fatalf("file mode = %o, want %o", got, 0o600)
	}
}

func TestAppendRejectsTimestampOutsideRFC3339NanoRange(t *testing.T) {
	store, err := Create(t.TempDir(), testHeader(t))
	if err != nil {
		t.Fatal(err)
	}
	path := store.Path()
	defer store.Close()
	before := readFile(t, path)

	message := model.Message{
		Role:      model.RoleUser,
		Blocks:    []model.Block{{Type: model.BlockText, Text: "out of range"}},
		CreatedAt: time.Date(10000, time.January, 1, 0, 0, 0, 0, time.UTC),
	}
	err = store.Append(context.Background(), message)
	if !errors.Is(err, ErrInvalidSession) {
		t.Fatalf("Append() error = %v, want ErrInvalidSession", err)
	}
	if after := readFile(t, path); !bytes.Equal(after, before) {
		t.Fatal("Append() wrote an out-of-range timestamp")
	}
	if got := store.Messages(); len(got) != 0 {
		t.Fatalf("Messages() = %#v, want no rejected message", got)
	}

	valid := model.Message{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "valid"}}, CreatedAt: time.Unix(2, 0).UTC()}
	if err := store.Append(context.Background(), valid); err != nil {
		t.Fatalf("Append() after timestamp rejection = %v", err)
	}
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	reopened, warnings, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer reopened.Close()
	if len(warnings) != 0 || len(reopened.Messages()) != 1 || reopened.Messages()[0].Text() != "valid" {
		t.Fatalf("reopened state: warnings=%#v messages=%#v", warnings, reopened.Messages())
	}
}

func TestStorePoisonedAfterDurableAppendFailure(t *testing.T) {
	for _, test := range []struct {
		name      string
		writeFail bool
		syncFail  bool
	}{
		{name: "write failure", writeFail: true},
		{name: "sync failure", syncFail: true},
	} {
		t.Run(test.name, func(t *testing.T) {
			store, err := Create(t.TempDir(), testHeader(t))
			if err != nil {
				t.Fatal(err)
			}
			defer store.Close()

			injected := errors.New("injected durable failure")
			writer := &faultingDurableWriter{file: store.file}
			if test.writeFail {
				writer.writeErr = injected
			}
			if test.syncFail {
				writer.syncErr = injected
			}
			store.writer = writer

			message := model.Message{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "hello"}}, CreatedAt: time.Unix(2, 0).UTC()}
			firstErr := store.Append(context.Background(), message)
			if !errors.Is(firstErr, ErrFatalPersistence) || !errors.Is(firstErr, injected) {
				t.Fatalf("first Append() error = %v, want fatal persistence wrapping injected failure", firstErr)
			}
			writes, syncs := writer.writes, writer.syncs

			secondErr := store.Append(context.Background(), message)
			if secondErr != firstErr {
				t.Fatalf("second Append() error = %p, want original fatal error %p", secondErr, firstErr)
			}
			if !errors.Is(secondErr, ErrFatalPersistence) || !errors.Is(secondErr, injected) {
				t.Fatalf("second Append() error = %v, want poisoned fatal persistence error", secondErr)
			}
			canceledCtx, cancel := context.WithCancel(context.Background())
			cancel()
			canceledErr := store.Append(canceledCtx, message)
			if canceledErr != firstErr {
				t.Fatalf("canceled Append() error = %p, want original fatal error %p", canceledErr, firstErr)
			}
			if !errors.Is(canceledErr, ErrFatalPersistence) || !errors.Is(canceledErr, injected) {
				t.Fatalf("canceled Append() after poison = %v, want fatal persistence error", canceledErr)
			}
			if writer.writes != writes || writer.syncs != syncs {
				t.Fatalf("poisoned store attempted another append: writes %d->%d syncs %d->%d", writes, writer.writes, syncs, writer.syncs)
			}
			if got := store.Messages(); len(got) != 0 {
				t.Fatalf("Messages() = %#v, want no committed messages", got)
			}
		})
	}
}

func TestWritePiRecordUsesLFAndSync(t *testing.T) {
	writer := &recordingDurableWriter{}
	header := piHeader{Type: "session", Version: PiSessionVersion, ID: "550e8400-e29b-41d4-a716-446655440000", Timestamp: time.Unix(1, 0).UTC().Format(time.RFC3339Nano), CWD: "/workspace"}
	written, err := writePiRecord(writer, header)
	if err != nil {
		t.Fatal(err)
	}
	if writer.syncs != 1 || written != int64(len(writer.data)) || !bytes.HasSuffix(writer.data, []byte{'\n'}) || bytes.Contains(writer.data, []byte{'\r'}) {
		t.Fatalf("write result: syncs=%d written=%d data=%q", writer.syncs, written, writer.data)
	}
}

func TestOpenRepairsMissingFinalLFBeforeAppend(t *testing.T) {
	header := testHeader(t)
	store, err := Create(t.TempDir(), header)
	if err != nil {
		t.Fatal(err)
	}
	firstMessage := model.Message{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "first"}}, CreatedAt: time.Unix(2, 0).UTC()}
	if err := store.Append(context.Background(), firstMessage); err != nil {
		t.Fatal(err)
	}
	path := store.Path()
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}

	complete := readFile(t, path)
	if !bytes.HasSuffix(complete, []byte{'\n'}) {
		t.Fatalf("created session is not LF terminated: %q", complete)
	}
	withoutFinalLF := append([]byte(nil), complete[:len(complete)-1]...)
	if err := os.WriteFile(path, withoutFinalLF, 0o600); err != nil {
		t.Fatal(err)
	}

	gotHeader, err := ReadHeader(path)
	if err != nil {
		t.Fatal(err)
	}
	if gotHeader != header {
		t.Fatalf("ReadHeader() = %#v, want %#v", gotHeader, header)
	}
	if afterRead := readFile(t, path); !bytes.Equal(afterRead, withoutFinalLF) {
		t.Fatal("ReadHeader repaired the missing final LF")
	}

	reopened, warnings, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	afterOpen := readFile(t, path)
	fileBytesAfterOpen := reopened.fileBytes
	secondMessage := model.Message{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "second"}}, CreatedAt: time.Unix(3, 0).UTC()}
	if err := reopened.Append(context.Background(), secondMessage); err != nil {
		_ = reopened.Close()
		t.Fatal(err)
	}
	if err := reopened.Close(); err != nil {
		t.Fatal(err)
	}

	reopenedAgain, secondWarnings, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer reopenedAgain.Close()
	if len(secondWarnings) != 0 {
		t.Fatalf("warnings after delimiter repair and append = %#v", secondWarnings)
	}
	if got := reopenedAgain.Messages(); len(got) != 2 || got[0].Text() != "first" || got[1].Text() != "second" {
		t.Fatalf("reopened Messages() = %#v", got)
	}

	wantAfterOpen := append(append([]byte(nil), withoutFinalLF...), '\n')
	if !bytes.Equal(afterOpen, wantAfterOpen) {
		t.Fatalf("Open() repair = %q, want exactly one appended LF", afterOpen)
	}
	if len(warnings) != 1 || !strings.Contains(warnings[0].Message, "missing final session delimiter") {
		t.Fatalf("Open() warnings = %#v, want missing-delimiter repair warning", warnings)
	}
	if fileBytesAfterOpen != int64(len(afterOpen)) {
		t.Fatalf("fileBytes after repair = %d, want %d", fileBytesAfterOpen, len(afterOpen))
	}
}

func TestOpenTruncatesIncompleteFinalLineWithWarning(t *testing.T) {
	store, err := Create(t.TempDir(), testHeader(t))
	if err != nil {
		t.Fatal(err)
	}
	message := model.Message{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "hello"}}, CreatedAt: time.Unix(2, 0).UTC()}
	if err := store.Append(context.Background(), message); err != nil {
		t.Fatal(err)
	}
	wantMessages := store.Messages()
	path := store.Path()
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	appendFixture(t, path, `{"type":"message","id":"12345678","parentId":`)

	reopened, warnings, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	if len(warnings) != 1 {
		t.Fatalf("warnings = %v, want 1", warnings)
	}
	if got := reopened.Messages(); !reflect.DeepEqual(got, wantMessages) {
		t.Fatalf("Messages() = %#v, want %#v", got, wantMessages)
	}
	if err := reopened.Close(); err != nil {
		t.Fatal(err)
	}

	contents := readFile(t, path)
	if bytes.Count(contents, []byte{'\n'}) != 3 || !bytes.HasSuffix(contents, []byte{'\n'}) {
		t.Fatalf("session file was not truncated to complete records: %q", contents)
	}
	reopenedAgain, secondWarnings, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer reopenedAgain.Close()
	if len(secondWarnings) != 0 {
		t.Fatalf("unexpected warnings after durable truncation: %v", secondWarnings)
	}
}

func TestOpenRejectsMalformedNonFinalLineWithoutMutation(t *testing.T) {
	store, err := Create(t.TempDir(), testHeader(t))
	if err != nil {
		t.Fatal(err)
	}
	path := store.Path()
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	appendFixture(t, path, "not-json\n"+piUnknownEntryLine("12345678", nil)+"\n")
	before := readFile(t, path)

	if _, _, err := Open(path); !errors.Is(err, ErrInvalidSession) {
		t.Fatalf("Open() error = %v, want ErrInvalidSession", err)
	}
	if after := readFile(t, path); !bytes.Equal(after, before) {
		t.Fatal("malformed non-final line was mutated")
	}
}

func TestOpenRejectsCompleteInvalidFinalJSONWithoutMutation(t *testing.T) {
	store, err := Create(t.TempDir(), testHeader(t))
	if err != nil {
		t.Fatal(err)
	}
	path := store.Path()
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	appendFixture(t, path, "not-json")
	before := readFile(t, path)

	if _, _, err := Open(path); !errors.Is(err, ErrInvalidSession) {
		t.Fatalf("Open() error = %v, want ErrInvalidSession", err)
	}
	if after := readFile(t, path); !bytes.Equal(after, before) {
		t.Fatal("complete invalid final record was mutated")
	}
}

func TestOpenValidatesPiEntryBaseBeforeMutation(t *testing.T) {
	validHeader := `{"type":"session","version":3,"id":"550e8400-e29b-41d4-a716-446655440000","timestamp":"1970-01-01T00:00:01Z","cwd":"/workspace"}`
	cases := map[string][]string{
		"invalid entry id": {
			`{"type":"future_entry","id":"NOT-HEX!","parentId":null,"timestamp":"1970-01-01T00:00:02Z"}`,
		},
		"duplicate entry id": {
			`{"type":"future_entry","id":"11111111","parentId":null,"timestamp":"1970-01-01T00:00:02Z"}`,
			`{"type":"future_entry","id":"11111111","parentId":"11111111","timestamp":"1970-01-01T00:00:03Z"}`,
		},
		"invalid entry timestamp": {
			`{"type":"future_entry","id":"11111111","parentId":null,"timestamp":"not-a-time"}`,
		},
		"forward parent": {
			`{"type":"future_entry","id":"11111111","parentId":"22222222","timestamp":"1970-01-01T00:00:02Z"}`,
			`{"type":"future_entry","id":"22222222","parentId":null,"timestamp":"1970-01-01T00:00:03Z"}`,
		},
	}
	for name, entries := range cases {
		t.Run(name, func(t *testing.T) {
			content := validHeader + "\n" + strings.Join(entries, "\n") + "\n"
			path := writeFixture(t, content)
			before := readFile(t, path)
			if _, _, err := Open(path); !errors.Is(err, ErrInvalidSession) {
				t.Fatalf("Open() error = %v, want ErrInvalidSession", err)
			}
			if after := readFile(t, path); !bytes.Equal(after, before) {
				t.Fatal("invalid session was mutated")
			}
		})
	}
}

func TestOpenPreservesUnknownPiEntriesAndAppendsBeneathLeaf(t *testing.T) {
	header := testHeader(t)
	store, err := Create(t.TempDir(), header)
	if err != nil {
		t.Fatal(err)
	}
	path := store.Path()
	decoded := decodeFixture(t, path)
	parent := decoded.Entries[0].ID
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	unknown := piUnknownEntryLine("12345678", &parent)
	appendFixture(t, path, unknown+"\n")

	reopened, warnings, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	if len(warnings) != 1 || !strings.Contains(warnings[0].Message, "12345678") {
		t.Fatalf("warnings = %#v, want active unknown-entry warning", warnings)
	}
	if err := reopened.Append(context.Background(), model.Message{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "after unknown"}}, CreatedAt: time.Unix(3, 0).UTC()}); err != nil {
		t.Fatal(err)
	}
	if err := reopened.Close(); err != nil {
		t.Fatal(err)
	}

	after := readFile(t, path)
	if !bytes.Contains(after, []byte(unknown)) {
		t.Fatal("unknown entry was not preserved")
	}
	decoded = decodeFixture(t, path)
	last := decoded.Entries[len(decoded.Entries)-1]
	if last.ParentID == nil || *last.ParentID != "12345678" {
		t.Fatalf("appended parent = %v, want unknown leaf", last.ParentID)
	}
}

func TestOpenRepairsDanglingToolCallDurably(t *testing.T) {
	path := createSessionWithDanglingCall(t)
	store, warnings, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	messages := store.Messages()
	last := messages[len(messages)-1]
	if len(warnings) != 1 || last.Role != model.RoleTool || len(last.Blocks) != 1 || !last.Blocks[0].IsError || last.Blocks[0].ToolCallID != "call-1" {
		t.Fatalf("dangling call was not repaired: warnings=%v last=%#v", warnings, last)
	}
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}

	reopened, reopenedWarnings, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer reopened.Close()
	if len(reopenedWarnings) != 0 {
		t.Fatalf("repair was not durable: warnings=%v", reopenedWarnings)
	}
	if got, want := len(reopened.Messages()), len(messages); got != want {
		t.Fatalf("reopened message count = %d, want %d", got, want)
	}
	decoded := decodeFixture(t, path)
	repair := decoded.Entries[len(decoded.Entries)-1].Message
	if repair == nil || repair.Role != "toolResult" || repair.IsError == nil || !*repair.IsError {
		t.Fatalf("persisted repair = %#v", repair)
	}
}

func TestStoreAppendAfterCloseFails(t *testing.T) {
	store, err := Create(t.TempDir(), testHeader(t))
	if err != nil {
		t.Fatal(err)
	}
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	if err := store.Close(); err != nil {
		t.Fatalf("second Close() = %v", err)
	}

	err = store.Append(context.Background(), model.Message{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "hello"}}, CreatedAt: time.Unix(2, 0).UTC()})
	if !errors.Is(err, errSessionClosed) {
		t.Fatalf("Append() error = %v, want errSessionClosed", err)
	}
	if got := store.Messages(); len(got) != 0 {
		t.Fatalf("Messages() after failed append = %#v, want empty", got)
	}
}

func TestStoreAppendHonorsCanceledContext(t *testing.T) {
	store, err := Create(t.TempDir(), testHeader(t))
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()

	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	err = store.Append(ctx, model.Message{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "hello"}}, CreatedAt: time.Unix(2, 0).UTC()})
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("Append() error = %v, want context.Canceled", err)
	}
	if got := store.Messages(); len(got) != 0 {
		t.Fatalf("Messages() after canceled append = %#v, want empty", got)
	}
}

func TestNewMemoryUsesCurrentVersion(t *testing.T) {
	header := testHeader(t)
	header.Version = 0
	store := NewMemory(header)
	defer store.Close()
	if got := store.Header().Version; got != CurrentVersion {
		t.Fatalf("Header().Version = %d, want %d", got, CurrentVersion)
	}
}

func TestMessagesReturnsIndependentSlices(t *testing.T) {
	header := testHeader(t)
	message := model.Message{Role: model.RoleAssistant, Blocks: []model.Block{
		{Type: model.BlockText, Text: "hello"},
		{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "read", Arguments: json.RawMessage(`{"path":"README.md"}`)},
	}, CreatedAt: time.Unix(2, 0).UTC(), FinishReason: model.FinishToolCalls, Usage: &model.Usage{InputTokens: 1, OutputTokens: 2}}

	cases := map[string]func(t *testing.T) Session{
		"memory": func(t *testing.T) Session {
			t.Helper()
			store := NewMemory(header)
			if err := store.Append(context.Background(), message); err != nil {
				t.Fatal(err)
			}
			return store
		},
		"store": func(t *testing.T) Session {
			t.Helper()
			store, err := Create(t.TempDir(), header)
			if err != nil {
				t.Fatal(err)
			}
			if err := store.Append(context.Background(), message); err != nil {
				t.Fatal(err)
			}
			return store
		},
	}

	for name, create := range cases {
		t.Run(name, func(t *testing.T) {
			store := create(t)
			defer store.Close()

			first := store.Messages()
			first[0].Blocks[0].Text = "mutated"
			first[0].Blocks[1].Arguments[0] = '['
			first[0].Usage.InputTokens = 99
			_ = append(first, model.Message{})

			second := store.Messages()
			if got, want := len(second), 1; got != want {
				t.Fatalf("len(Messages()) = %d, want %d", got, want)
			}
			if got := second[0].Blocks[0].Text; got != "hello" {
				t.Fatalf("Blocks[0].Text = %q, want hello", got)
			}
			if got := string(second[0].Blocks[1].Arguments); got != `{"path":"README.md"}` {
				t.Fatalf("Arguments = %q", got)
			}
			if got := second[0].Usage.InputTokens; got != 1 {
				t.Fatalf("Usage.InputTokens = %d, want 1", got)
			}
		})
	}
}

func createSessionWithDanglingCall(t *testing.T) string {
	t.Helper()
	store, err := Create(t.TempDir(), testHeader(t))
	if err != nil {
		t.Fatal(err)
	}
	assistant := model.Message{Role: model.RoleAssistant, CreatedAt: time.Unix(2, 0).UTC(), FinishReason: model.FinishToolCalls, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "read", Arguments: json.RawMessage(`{"path":"README.md"}`)}}}
	if err := store.Append(context.Background(), assistant); err != nil {
		t.Fatal(err)
	}
	path := store.Path()
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	return path
}

func testHeader(t *testing.T) Header {
	t.Helper()
	return Header{
		Version: CurrentVersion, ID: "550e8400-e29b-41d4-a716-446655440000",
		Workspace: t.TempDir(), Provider: "openai-compatible", Profile: "local",
		Model: "test-model", CreatedAt: time.Unix(1, 0).UTC(),
	}
}

func readJSONLines(t *testing.T, path string) [][]byte {
	t.Helper()
	contents := readFile(t, path)
	if len(contents) == 0 || contents[len(contents)-1] != '\n' {
		t.Fatalf("file is not LF terminated: %q", contents)
	}
	return bytes.Split(bytes.TrimSuffix(contents, []byte{'\n'}), []byte{'\n'})
}

func assertJSONEqual(t *testing.T, raw []byte, want any) {
	t.Helper()
	var got any
	if err := json.Unmarshal(raw, &got); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("JSON = %#v, want %#v; raw=%s", got, want, raw)
	}
}

func writeFixture(t *testing.T, content string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), "session.jsonl")
	if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}
	return path
}

func appendFixture(t *testing.T, path, content string) {
	t.Helper()
	file, err := os.OpenFile(path, os.O_APPEND|os.O_WRONLY, 0)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := file.WriteString(content); err != nil {
		_ = file.Close()
		t.Fatal(err)
	}
	if err := file.Close(); err != nil {
		t.Fatal(err)
	}
}

func readFile(t *testing.T, path string) []byte {
	t.Helper()
	contents, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	return contents
}

func decodeFixture(t *testing.T, path string) piFile {
	t.Helper()
	file, err := os.Open(path)
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

func piUnknownEntryLine(id string, parent *string) string {
	parentJSON := "null"
	if parent != nil {
		parentJSON = `"` + *parent + `"`
	}
	return `{"type":"future_entry","id":"` + id + `","parentId":` + parentJSON + `,"timestamp":"1970-01-01T00:00:02Z","futureField":{"preserve":true}}`
}

func validTestEntryID(id string) bool {
	return regexp.MustCompile(`^[0-9a-f]{8}$`).MatchString(id)
}

type faultingDurableWriter struct {
	file     *os.File
	writeErr error
	syncErr  error
	writes   int
	syncs    int
}

func (w *faultingDurableWriter) Write(data []byte) (int, error) {
	w.writes++
	if w.writeErr != nil {
		return 0, w.writeErr
	}
	return w.file.Write(data)
}

func (w *faultingDurableWriter) Sync() error {
	w.syncs++
	if w.syncErr != nil {
		return w.syncErr
	}
	return w.file.Sync()
}

type recordingDurableWriter struct {
	data  []byte
	syncs int
}

func (w *recordingDurableWriter) Write(data []byte) (int, error) {
	w.data = append(w.data, data...)
	return len(data), nil
}

func (w *recordingDurableWriter) Sync() error {
	w.syncs++
	return nil
}
