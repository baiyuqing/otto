package session

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/model"
)

func TestStoreRoundTrip(t *testing.T) {
	root := t.TempDir()
	header := Header{Version: 1, ID: "session-1", Workspace: "/tmp/project", Provider: "openai-compatible", Profile: "local", Model: "test-model", CreatedAt: time.Unix(1, 0).UTC()}
	store, err := Create(root, header)
	if err != nil {
		t.Fatal(err)
	}
	message := model.Message{ID: "msg-1", Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "hello"}}, CreatedAt: time.Unix(2, 0).UTC()}
	if err := store.Append(context.Background(), message); err != nil {
		t.Fatal(err)
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
	if len(warnings) != 0 || !reflect.DeepEqual(reopened.Messages(), []model.Message{message}) {
		t.Fatalf("unexpected reopen result: warnings=%v messages=%#v", warnings, reopened.Messages())
	}
}

func TestOpenRoundTripsZeroBlockAssistantMessage(t *testing.T) {
	root := t.TempDir()
	header := Header{Version: 1, ID: "session-1", Workspace: "/tmp/project", Provider: "openai-compatible", Model: "test-model", CreatedAt: time.Unix(1, 0).UTC()}
	store, err := Create(root, header)
	if err != nil {
		t.Fatal(err)
	}
	message := model.Message{ID: "assistant-1", Role: model.RoleAssistant, Blocks: []model.Block{}, CreatedAt: time.Unix(2, 0).UTC(), FinishReason: model.FinishUnknown}
	if err := store.Append(context.Background(), message); err != nil {
		t.Fatal(err)
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
	if len(warnings) != 0 || !reflect.DeepEqual(reopened.Messages(), []model.Message{message}) {
		t.Fatalf("unexpected reopen result: warnings=%v messages=%#v", warnings, reopened.Messages())
	}
}

func TestOpenRepairsDanglingToolCall(t *testing.T) {
	path := createSessionWithDanglingCall(t)
	store, warnings, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	messages := store.Messages()
	last := messages[len(messages)-1]
	if len(warnings) != 1 || last.Role != model.RoleTool || !last.Blocks[0].IsError || last.Blocks[0].ToolCallID != "call-1" {
		t.Fatalf("dangling call was not repaired: warnings=%v last=%#v", warnings, last)
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

	store, err := Create(root, Header{Version: 1, ID: "session-1", Workspace: symlinkWorkspace, Provider: "openai-compatible", Model: "test-model", CreatedAt: time.Unix(1, 0).UTC()})
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
	wantPath := filepath.Join(root, workspaceKey, "session-1.jsonl")
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
			store, err := Create(t.TempDir(), Header{Version: 1, ID: "session-1", Workspace: t.TempDir(), Provider: "openai-compatible", Model: "test-model", CreatedAt: time.Unix(1, 0).UTC()})
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

			message := model.Message{ID: "msg-1", Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "hello"}}, CreatedAt: time.Unix(2, 0).UTC()}
			firstErr := store.Append(context.Background(), message)
			if !errors.Is(firstErr, ErrFatalPersistence) || !errors.Is(firstErr, injected) {
				t.Fatalf("first Append() error = %v, want fatal persistence wrapping injected failure", firstErr)
			}
			writes, syncs := writer.writes, writer.syncs

			secondErr := store.Append(context.Background(), message)
			if !errors.Is(secondErr, ErrFatalPersistence) || !errors.Is(secondErr, injected) {
				t.Fatalf("second Append() error = %v, want poisoned fatal persistence error", secondErr)
			}
			canceledCtx, cancel := context.WithCancel(context.Background())
			cancel()
			canceledErr := store.Append(canceledCtx, message)
			if !errors.Is(canceledErr, ErrFatalPersistence) || !errors.Is(canceledErr, injected) {
				t.Fatalf("canceled Append() after poison = %v, want fatal persistence error", canceledErr)
			}
			if writer.writes != writes || writer.syncs != syncs {
				t.Fatalf("poisoned store attempted another durable append: writes %d->%d syncs %d->%d", writes, writer.writes, syncs, writer.syncs)
			}
			if got := store.Messages(); len(got) != 0 {
				t.Fatalf("Messages() = %#v, want no committed messages", got)
			}
		})
	}
}

func TestOpenTruncatesMalformedFinalLineWithWarning(t *testing.T) {
	root := t.TempDir()
	store, err := Create(root, Header{Version: 1, ID: "session-1", Workspace: root, Provider: "openai-compatible", Model: "test-model", CreatedAt: time.Unix(1, 0).UTC()})
	if err != nil {
		t.Fatal(err)
	}
	message := model.Message{ID: "msg-1", Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "hello"}}, CreatedAt: time.Unix(2, 0).UTC()}
	if err := store.Append(context.Background(), message); err != nil {
		t.Fatal(err)
	}
	path := store.Path()
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	file, err := os.OpenFile(path, os.O_APPEND|os.O_WRONLY, 0)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := file.WriteString(`{"type":"message","message":`); err != nil {
		t.Fatal(err)
	}
	if err := file.Close(); err != nil {
		t.Fatal(err)
	}

	reopened, warnings, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	reopened.Close()
	if len(warnings) != 1 {
		t.Fatalf("warnings = %v, want 1 warning", warnings)
	}
	if got := reopened.Messages(); !reflect.DeepEqual(got, []model.Message{message}) {
		t.Fatalf("Messages() = %#v, want %#v", got, []model.Message{message})
	}

	content, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Count(string(content), "\n") != 2 || !strings.HasSuffix(string(content), "\n") {
		t.Fatalf("session file was not truncated to two complete records: %q", string(content))
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

func TestOpenFailsMalformedNonFinalLine(t *testing.T) {
	path := filepath.Join(t.TempDir(), "session.jsonl")
	content := strings.Join([]string{
		marshalLine(t, Record{Type: "header", Header: &Header{Version: 1, ID: "session-1", Workspace: "/tmp/project", Provider: "openai-compatible", Model: "test-model", CreatedAt: time.Unix(1, 0).UTC()}}),
		"not-json",
		marshalLine(t, Record{Type: "message", Message: &model.Message{ID: "msg-1", Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "hello"}}, CreatedAt: time.Unix(2, 0).UTC()}}),
	}, "\n") + "\n"
	if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}

	if _, _, err := Open(path); err == nil {
		t.Fatal("expected malformed non-final line to fail")
	}
}

func TestOpenRejectsUnsupportedHeaderVersion(t *testing.T) {
	path := filepath.Join(t.TempDir(), "session.jsonl")
	content := marshalLine(t, Record{Type: "header", Header: &Header{Version: 2, ID: "session-1", Workspace: "/tmp/project", Provider: "openai-compatible", Model: "test-model", CreatedAt: time.Unix(1, 0).UTC()}}) + "\n"
	if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}

	if _, _, err := Open(path); err == nil {
		t.Fatal("expected unsupported header version to fail")
	}
}

func TestOpenRejectsMalformedHeaderEvenWhenFinal(t *testing.T) {
	path := filepath.Join(t.TempDir(), "session.jsonl")
	if err := os.WriteFile(path, []byte("not-json"), 0o600); err != nil {
		t.Fatal(err)
	}

	if _, _, err := Open(path); err == nil {
		t.Fatal("expected malformed header to fail")
	}
}

func TestOpenRejectsCompleteMalformedFinalJSONWithoutMutation(t *testing.T) {
	path := filepath.Join(t.TempDir(), "session.jsonl")
	header := marshalLine(t, Record{Type: recordTypeHeader, Header: &Header{Version: 1, ID: "session-1", Workspace: "/tmp/project", Provider: "openai-compatible", Model: "test-model", CreatedAt: time.Unix(1, 0).UTC()}})
	content := header + "\nnot-json"
	if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, _, err := Open(path); err == nil {
		t.Fatal("expected complete malformed final JSON to fail")
	}
	after, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if string(after) != content {
		t.Fatalf("invalid complete final record was mutated: %q", after)
	}
}

func TestOpenRejectsInvalidPersistedFixturesBeforeMutation(t *testing.T) {
	const timestamp = "1970-01-01T00:00:01Z"
	validHeader := `{"type":"header","header":{"version":1,"id":"session-1","workspace":"/tmp/project","provider":"openai-compatible","model":"test-model","created_at":"` + timestamp + `"}}`
	validUser := `{"type":"message","message":{"id":"user-1","role":"user","blocks":[{"type":"text","text":"hello"}],"created_at":"` + timestamp + `"}}`
	cases := map[string]string{
		"unknown record field":        strings.Replace(validHeader, `"type":"header"`, `"type":"header","extra":true`, 1),
		"unknown header field":        strings.Replace(validHeader, `"version":1`, `"version":1,"extra":true`, 1),
		"missing header version":      strings.Replace(validHeader, `"version":1,`, "", 1),
		"missing header id":           strings.Replace(validHeader, `"id":"session-1",`, "", 1),
		"missing header workspace":    strings.Replace(validHeader, `"workspace":"/tmp/project",`, "", 1),
		"missing header provider":     strings.Replace(validHeader, `"provider":"openai-compatible",`, "", 1),
		"missing header model":        strings.Replace(validHeader, `"model":"test-model",`, "", 1),
		"zero header timestamp":       strings.Replace(validHeader, `"created_at":"`+timestamp+`"`, `"created_at":"0001-01-01T00:00:00Z"`, 1),
		"header with message payload": strings.TrimSuffix(validHeader, "}") + `,"message":{"id":"unexpected"}}`,
		"unknown message field":       validHeader + "\n" + strings.Replace(validUser, `"id":"user-1"`, `"id":"user-1","extra":true`, 1),
		"missing message id":          validHeader + "\n" + strings.Replace(validUser, `"id":"user-1",`, "", 1),
		"invalid message role":        validHeader + "\n" + strings.Replace(validUser, `"role":"user"`, `"role":"system"`, 1),
		"zero message timestamp":      validHeader + "\n" + strings.Replace(validUser, `"created_at":"`+timestamp+`"`, `"created_at":"0001-01-01T00:00:00Z"`, 1),
		"empty user message blocks":   validHeader + "\n" + strings.Replace(validUser, `[{"type":"text","text":"hello"}]`, `[]`, 1),
		"empty tool message blocks":   validHeader + "\n" + `{"type":"message","message":{"id":"tool-1","role":"tool","blocks":[],"created_at":"` + timestamp + `"}}`,
		"unknown block field":         validHeader + "\n" + strings.Replace(validUser, `"text":"hello"`, `"text":"hello","extra":true`, 1),
		"user tool-call block":        validHeader + "\n" + strings.Replace(validUser, `{"type":"text","text":"hello"}`, `{"type":"tool_call","tool_call_id":"call-1","tool_name":"read","arguments":{}}`, 1),
		"assistant tool-result block": validHeader + "\n" + `{"type":"message","message":{"id":"assistant-1","role":"assistant","blocks":[{"type":"tool_result","tool_call_id":"call-1","tool_name":"read"}],"created_at":"` + timestamp + `","finish_reason":"stop"}}`,
		"tool text block":             validHeader + "\n" + `{"type":"message","message":{"id":"tool-1","role":"tool","blocks":[{"type":"text","text":"bad"}],"created_at":"` + timestamp + `"}}`,
		"invalid finish reason":       validHeader + "\n" + `{"type":"message","message":{"id":"assistant-1","role":"assistant","blocks":[{"type":"text","text":"hello"}],"created_at":"` + timestamp + `","finish_reason":"bad"}}`,
		"missing assistant finish":    validHeader + "\n" + `{"type":"message","message":{"id":"assistant-1","role":"assistant","blocks":[{"type":"text","text":"hello"}],"created_at":"` + timestamp + `"}}`,
		"negative usage":              validHeader + "\n" + `{"type":"message","message":{"id":"assistant-1","role":"assistant","blocks":[{"type":"text","text":"hello"}],"created_at":"` + timestamp + `","finish_reason":"stop","usage":{"input_tokens":-1,"output_tokens":0}}}`,
		"unknown usage field":         validHeader + "\n" + `{"type":"message","message":{"id":"assistant-1","role":"assistant","blocks":[{"type":"text","text":"hello"}],"created_at":"` + timestamp + `","finish_reason":"stop","usage":{"input_tokens":1,"output_tokens":0,"extra":true}}}`,
		"missing tool-call id":        validHeader + "\n" + `{"type":"message","message":{"id":"assistant-1","role":"assistant","blocks":[{"type":"tool_call","tool_name":"read","arguments":{}}],"created_at":"` + timestamp + `","finish_reason":"tool_calls"}}`,
		"missing tool-call name":      validHeader + "\n" + `{"type":"message","message":{"id":"assistant-1","role":"assistant","blocks":[{"type":"tool_call","tool_call_id":"call-1","arguments":{}}],"created_at":"` + timestamp + `","finish_reason":"tool_calls"}}`,
		"null tool-call arguments":    validHeader + "\n" + `{"type":"message","message":{"id":"assistant-1","role":"assistant","blocks":[{"type":"tool_call","tool_call_id":"call-1","tool_name":"read","arguments":null}],"created_at":"` + timestamp + `","finish_reason":"tool_calls"}}`,
		"non-object tool arguments":   validHeader + "\n" + `{"type":"message","message":{"id":"assistant-1","role":"assistant","blocks":[{"type":"tool_call","tool_call_id":"call-1","tool_name":"read","arguments":"not-an-object"}],"created_at":"` + timestamp + `","finish_reason":"tool_calls"}}`,
		"missing tool-result id":      validHeader + "\n" + `{"type":"message","message":{"id":"tool-1","role":"tool","blocks":[{"type":"tool_result","tool_name":"read"}],"created_at":"` + timestamp + `"}}`,
		"missing tool-result name":    validHeader + "\n" + `{"type":"message","message":{"id":"tool-1","role":"tool","blocks":[{"type":"tool_result","tool_call_id":"call-1"}],"created_at":"` + timestamp + `"}}`,
		"orphan tool result":          validHeader + "\n" + `{"type":"message","message":{"id":"tool-1","role":"tool","blocks":[{"type":"tool_result","tool_call_id":"call-1","tool_name":"read"}],"created_at":"` + timestamp + `"}}`,
		"message with header payload": validHeader + "\n" + strings.TrimSuffix(validUser, "}") + `,"header":{"version":1}}`,
		"header after first line":     validHeader + "\n" + validHeader,
	}

	for name, content := range cases {
		t.Run(name, func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "session.jsonl")
			original := content + "\n"
			if err := os.WriteFile(path, []byte(original), 0o600); err != nil {
				t.Fatal(err)
			}
			if _, _, err := Open(path); err == nil {
				t.Fatal("expected invalid persisted fixture to fail")
			}
			after, err := os.ReadFile(path)
			if err != nil {
				t.Fatal(err)
			}
			if string(after) != original {
				t.Fatalf("invalid fixture was mutated: before=%q after=%q", original, after)
			}
		})
	}
}

func TestOpenRejectsSemanticallyInvalidFinalRecord(t *testing.T) {
	header := marshalLine(t, Record{Type: recordTypeHeader, Header: &Header{Version: 1, ID: "session-1", Workspace: "/tmp/project", Provider: "openai-compatible", Model: "test-model", CreatedAt: time.Unix(1, 0).UTC()}})
	cases := map[string]string{
		"missing message payload": `{"type":"message"}`,
		"unsupported record type": `{"type":"warning"}`,
	}

	for name, finalLine := range cases {
		t.Run(name, func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "session.jsonl")
			content := header + "\n" + finalLine + "\n"
			if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
				t.Fatal(err)
			}

			if _, _, err := Open(path); err == nil {
				t.Fatal("expected semantically invalid final record to fail")
			}
		})
	}
}

func TestPersistenceWireSchemaIsExplicitAllowlist(t *testing.T) {
	recordType := reflect.TypeOf(persistedRecord{})
	if field, ok := recordType.FieldByName("Header"); !ok {
		t.Fatal("persistedRecord.Header missing")
	} else if field.Type == reflect.TypeOf((*Header)(nil)) {
		t.Fatal("persistedRecord.Header must not embed session.Header directly")
	}
	if field, ok := recordType.FieldByName("Message"); !ok {
		t.Fatal("persistedRecord.Message missing")
	} else if field.Type == reflect.TypeOf((*model.Message)(nil)) {
		t.Fatal("persistedRecord.Message must not embed model.Message directly")
	}
	assertJSONTags(t, recordType, "type", "header,omitempty", "message,omitempty")
	assertJSONTags(t, reflect.TypeOf(persistedHeader{}), "version", "id", "workspace", "provider", "profile,omitempty", "model", "created_at")
	assertJSONTags(t, reflect.TypeOf(persistedMessage{}), "id", "role", "blocks", "created_at", "finish_reason,omitempty", "usage,omitempty")
	assertJSONTags(t, reflect.TypeOf(persistedBlock{}), "type", "text,omitempty", "tool_call_id,omitempty", "tool_name,omitempty", "arguments,omitempty", "is_error,omitempty")
	assertJSONTags(t, reflect.TypeOf(persistedUsage{}), "input_tokens", "output_tokens")

	messageType := reflect.TypeOf(persistedMessage{})
	if field, ok := messageType.FieldByName("Blocks"); !ok {
		t.Fatal("persistedMessage.Blocks missing")
	} else if field.Type == reflect.TypeOf([]model.Block(nil)) {
		t.Fatal("persistedMessage.Blocks must not embed model.Block directly")
	}
	if field, ok := messageType.FieldByName("Usage"); !ok {
		t.Fatal("persistedMessage.Usage missing")
	} else if field.Type == reflect.TypeOf((*model.Usage)(nil)) {
		t.Fatal("persistedMessage.Usage must not embed model.Usage directly")
	}
}

func TestStoreAppendAfterCloseFails(t *testing.T) {
	store, err := Create(t.TempDir(), Header{Version: 1, ID: "session-1", Workspace: "/tmp/project", Provider: "openai-compatible", Model: "test-model", CreatedAt: time.Unix(1, 0).UTC()})
	if err != nil {
		t.Fatal(err)
	}
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}

	err = store.Append(context.Background(), model.Message{ID: "msg-1", Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "hello"}}, CreatedAt: time.Unix(2, 0).UTC()})
	if err == nil {
		t.Fatal("expected append after close to fail")
	}
	if got := store.Messages(); len(got) != 0 {
		t.Fatalf("Messages() after failed append = %#v, want empty", got)
	}
}

func TestStoreAppendHonorsCanceledContext(t *testing.T) {
	store, err := Create(t.TempDir(), Header{Version: 1, ID: "session-1", Workspace: "/tmp/project", Provider: "openai-compatible", Model: "test-model", CreatedAt: time.Unix(1, 0).UTC()})
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()

	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	err = store.Append(ctx, model.Message{ID: "msg-1", Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "hello"}}, CreatedAt: time.Unix(2, 0).UTC()})
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("Append() error = %v, want %v", err, context.Canceled)
	}
	if got := store.Messages(); len(got) != 0 {
		t.Fatalf("Messages() after canceled append = %#v, want empty", got)
	}
}

func TestMessagesReturnsIndependentSlices(t *testing.T) {
	header := Header{Version: 1, ID: "session-1", Workspace: "/tmp/project", Provider: "openai-compatible", Model: "test-model", CreatedAt: time.Unix(1, 0).UTC()}
	message := model.Message{ID: "msg-1", Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "hello"}}, CreatedAt: time.Unix(2, 0).UTC(), Usage: &model.Usage{InputTokens: 1, OutputTokens: 2}}

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
			first[0].Usage.InputTokens = 99
			_ = append(first, model.Message{ID: "msg-2"})

			second := store.Messages()
			if got, want := len(second), 1; got != want {
				t.Fatalf("len(Messages()) = %d, want %d", got, want)
			}
			if got := second[0].Blocks[0].Text; got != "hello" {
				t.Fatalf("Blocks[0].Text = %q, want %q", got, "hello")
			}
			if got := second[0].Usage.InputTokens; got != 1 {
				t.Fatalf("Usage.InputTokens = %d, want %d", got, 1)
			}
		})
	}
}

func createSessionWithDanglingCall(t *testing.T) string {
	t.Helper()
	store, err := Create(t.TempDir(), Header{Version: 1, ID: "dangling", Workspace: t.TempDir(), Provider: "openai-compatible", Model: "test", CreatedAt: time.Unix(1, 0).UTC()})
	if err != nil {
		t.Fatal(err)
	}
	assistant := model.Message{ID: "assistant-1", Role: model.RoleAssistant, CreatedAt: time.Unix(2, 0).UTC(), FinishReason: model.FinishToolCalls, Blocks: []model.Block{{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "read", Arguments: json.RawMessage(`{"path":"README.md"}`)}}}
	if err := store.Append(context.Background(), assistant); err != nil {
		t.Fatal(err)
	}
	path := store.Path()
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	return path
}

func marshalLine(t *testing.T, record Record) string {
	t.Helper()
	encoded, err := json.Marshal(record)
	if err != nil {
		t.Fatal(err)
	}
	return string(encoded)
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

func assertJSONTags(t *testing.T, typ reflect.Type, want ...string) {
	t.Helper()
	got := make([]string, 0, typ.NumField())
	for i := 0; i < typ.NumField(); i++ {
		got = append(got, typ.Field(i).Tag.Get("json"))
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("%s json tags = %v, want %v", typ.Name(), got, want)
	}
}
