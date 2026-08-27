package session

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestListReturnsRecentValidWorkspaceSessionsWithoutMutation(t *testing.T) {
	root := t.TempDir()
	workspace := t.TempDir()
	base := time.Unix(1_000, 0).UTC()

	var newestValid string
	for i := 0; i < 24; i++ {
		path := createListSession(t, root, workspace, fmt.Sprintf("session-%02d", i))
		setListMTime(t, path, base.Add(time.Duration(i)*time.Minute))
		newestValid = path
	}

	oldPath := writeOldOttoV1Session(t, root, workspace, "old-v1")
	corruptPath := writeCorruptJSONLSession(t, root, workspace, "corrupt")
	setListMTime(t, oldPath, base.Add(30*time.Minute))
	setListMTime(t, corruptPath, base.Add(31*time.Minute))
	beforeOld, beforeCorrupt := readFile(t, oldPath), readFile(t, corruptPath)

	result, err := List(context.Background(), root, workspace, newestValid, 20)
	if err != nil {
		t.Fatal(err)
	}
	if len(result.Sessions) != 20 || result.Skipped != 2 {
		t.Fatalf("result = %#v", result)
	}
	if !result.Sessions[0].Current {
		t.Fatalf("first = %#v", result.Sessions[0])
	}
	assertSortedNewestFirst(t, result.Sessions)
	if !bytes.Equal(readFile(t, oldPath), beforeOld) {
		t.Fatal("old v1 session was mutated")
	}
	if !bytes.Equal(readFile(t, corruptPath), beforeCorrupt) {
		t.Fatal("corrupt session was mutated")
	}
}

func TestListRejectsSymlinksAndOtherWorkspaces(t *testing.T) {
	root := t.TempDir()
	workspace := t.TempDir()
	valid := createListSession(t, root, workspace, "valid")
	directory := filepath.Dir(valid)
	if err := os.Symlink(valid, filepath.Join(directory, "linked.jsonl")); err != nil {
		t.Fatal(err)
	}
	createPiSessionAtPath(t, filepath.Join(directory, "other.jsonl"), t.TempDir(), "other")

	result, err := List(context.Background(), root, workspace, "", 20)
	if err != nil {
		t.Fatal(err)
	}
	if len(result.Sessions) != 1 || result.Sessions[0].Path != valid || result.Skipped != 2 {
		t.Fatalf("result = %#v", result)
	}
}

func TestListOrdersMTimeTiesDeterministically(t *testing.T) {
	root := t.TempDir()
	workspace := t.TempDir()
	first := createListSession(t, root, workspace, "aaa")
	second := createListSession(t, root, workspace, "bbb")
	modified := time.Unix(2_000, 0).UTC()
	setListMTime(t, first, modified)
	setListMTime(t, second, modified)

	result, err := List(context.Background(), root, workspace, "", 20)
	if err != nil {
		t.Fatal(err)
	}
	if got, want := []string{result.Sessions[0].Path, result.Sessions[1].Path}, []string{second, first}; fmt.Sprint(got) != fmt.Sprint(want) {
		t.Fatalf("paths = %v, want %v", got, want)
	}
}

func TestListMatchesCanonicalWorkspaceAndCurrentPath(t *testing.T) {
	root := t.TempDir()
	workspaceRoot := t.TempDir()
	canonicalWorkspace := filepath.Join(workspaceRoot, "workspace")
	if err := os.Mkdir(canonicalWorkspace, 0o700); err != nil {
		t.Fatal(err)
	}
	symlinkWorkspace := filepath.Join(workspaceRoot, "workspace-link")
	if err := os.Symlink(canonicalWorkspace, symlinkWorkspace); err != nil {
		t.Fatal(err)
	}

	valid := createListSession(t, root, symlinkWorkspace, "valid")
	currentPath := filepath.Join(t.TempDir(), "current.jsonl")
	if err := os.Symlink(valid, currentPath); err != nil {
		t.Fatal(err)
	}

	result, err := List(context.Background(), root, canonicalWorkspace, currentPath, 20)
	if err != nil {
		t.Fatal(err)
	}
	if len(result.Sessions) != 1 || !result.Sessions[0].Current || result.Sessions[0].Path != valid || result.Sessions[0].CWD != symlinkWorkspace {
		t.Fatalf("result = %#v", result)
	}
}

func TestListReturnsEmptyWhenWorkspaceDirectoryIsMissing(t *testing.T) {
	result, err := List(context.Background(), t.TempDir(), t.TempDir(), "", 20)
	if err != nil {
		t.Fatal(err)
	}
	if len(result.Sessions) != 0 || result.Skipped != 0 {
		t.Fatalf("result = %#v", result)
	}
}

func TestListFailsWhenSessionDirectoryCannotBeRead(t *testing.T) {
	root := filepath.Join(t.TempDir(), "sessions-root")
	if err := os.WriteFile(root, []byte("not a directory"), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err := List(context.Background(), root, t.TempDir(), "", 20); err == nil {
		t.Fatal("List() succeeded with an unreadable session directory")
	}
}

func TestListValidatesLimit(t *testing.T) {
	for _, limit := range []int{-1, 0, 21} {
		t.Run(fmt.Sprintf("limit=%d", limit), func(t *testing.T) {
			if _, err := List(context.Background(), t.TempDir(), t.TempDir(), "", limit); err == nil {
				t.Fatal("List() succeeded with invalid limit")
			}
		})
	}
}

func TestListRespectsContextCancellation(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	_, err := List(ctx, t.TempDir(), t.TempDir(), "", 20)
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("error = %v, want context.Canceled", err)
	}
}

func TestInspectDerivesActiveMetadataAndNameOverride(t *testing.T) {
	path := copyPiFixture(t, "tree.jsonl")
	modified := time.Unix(3_000, 0).UTC()
	setListMTime(t, path, modified)

	info, warnings, err := Inspect(context.Background(), path)
	if err != nil {
		t.Fatal(err)
	}
	if len(warnings) != 0 {
		t.Fatalf("warnings = %#v", warnings)
	}
	if info.Path != path || info.ID != "650e8400-e29b-41d4-a716-446655440000" || info.CWD != "/workspace" {
		t.Fatalf("info = %#v", info)
	}
	if info.Name != "Fixture tree" || info.LastUserText != "active branch" || info.MessageCount != 3 {
		t.Fatalf("info = %#v", info)
	}
	if info.Profile != "default" || info.Provider != "openai-compatible" || info.Model != "test-model" {
		t.Fatalf("info = %#v", info)
	}
	if !info.Created.Equal(time.Date(2026, time.August, 27, 9, 0, 0, 0, time.UTC)) || !info.Modified.Equal(modified) {
		t.Fatalf("info = %#v", info)
	}
}

func TestInspectUsesLastUserPreviewWhenNameIsMissing(t *testing.T) {
	info, warnings, err := Inspect(context.Background(), copyPiFixture(t, "linear.jsonl"))
	if err != nil {
		t.Fatal(err)
	}
	if len(warnings) != 0 {
		t.Fatalf("warnings = %#v", warnings)
	}
	if info.Name != "run the check" || info.LastUserText != "run the check" || info.MessageCount != 4 {
		t.Fatalf("info = %#v", info)
	}
	if info.Profile != "" || info.Provider != "openai-compatible" || info.Model != "test-model" {
		t.Fatalf("info = %#v", info)
	}
}

func TestInspectSanitizesPickerMetadata(t *testing.T) {
	path := filepath.Join(t.TempDir(), "sanitized.jsonl")
	writeListRecords(t, path,
		`{"type":"session","version":3,"id":"sanitized","timestamp":"2026-08-27T12:00:00Z","cwd":"/workspace"}`,
		`{"type":"custom","id":"71000001","parentId":null,"timestamp":"2026-08-27T12:00:01Z","customType":"otto.runtime","data":{"profile":"default","provider":"openai-compatible","model":"test-model"}}`,
		`{"type":"message","id":"71000002","parentId":"71000001","timestamp":"2026-08-27T12:00:02Z","message":{"role":"user","content":"hello\n\u001b[31msecret\tworld","timestamp":1}}`,
		`{"type":"session_info","id":"71000003","parentId":"71000002","timestamp":"2026-08-27T12:00:03Z","name":"picked\nname\u001b[31m"}`,
	)

	info, _, err := Inspect(context.Background(), path)
	if err != nil {
		t.Fatal(err)
	}
	if strings.ContainsAny(info.Name, "\n\r\t\x1b") || strings.ContainsAny(info.LastUserText, "\n\r\t\x1b") {
		t.Fatalf("info = %#v", info)
	}
	if info.Name != `picked name\x1b[31m` || info.LastUserText != `hello \x1b[31msecret world` {
		t.Fatalf("info = %#v", info)
	}
}

func TestInspectLeavesMissingFinalDelimiterUntouched(t *testing.T) {
	path := copyPiFixture(t, "linear.jsonl")
	before := bytes.TrimSuffix(readFile(t, path), []byte{'\n'})
	if err := os.WriteFile(path, before, 0o600); err != nil {
		t.Fatal(err)
	}

	info, warnings, err := Inspect(context.Background(), path)
	if err != nil {
		t.Fatal(err)
	}
	if len(warnings) != 0 || info.ID == "" {
		t.Fatalf("info = %#v warnings = %#v", info, warnings)
	}
	if after := readFile(t, path); !bytes.Equal(after, before) {
		t.Fatal("Inspect() repaired the missing delimiter")
	}
}

func TestInspectLeavesDanglingToolCallUntouched(t *testing.T) {
	path := createSessionWithDanglingCall(t)
	before := readFile(t, path)

	info, warnings, err := Inspect(context.Background(), path)
	if err != nil {
		t.Fatal(err)
	}
	if len(warnings) != 0 || info.MessageCount != 1 {
		t.Fatalf("info = %#v warnings = %#v", info, warnings)
	}
	if after := readFile(t, path); !bytes.Equal(after, before) {
		t.Fatal("Inspect() repaired the dangling tool call")
	}
}

func TestInspectRespectsContextCancellation(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	_, _, err := Inspect(ctx, copyPiFixture(t, "linear.jsonl"))
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("error = %v, want context.Canceled", err)
	}
}

func createListSession(t *testing.T, root, workspace, id string) string {
	t.Helper()
	store, err := Create(root, Header{Version: CurrentVersion, ID: id, Workspace: workspace, Provider: "openai-compatible", Profile: "default", Model: "test-model", CreatedAt: time.Unix(1, 0).UTC()})
	if err != nil {
		t.Fatal(err)
	}
	path := store.Path()
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	return path
}

func writeOldOttoV1Session(t *testing.T, root, workspace, id string) string {
	t.Helper()
	path := filepath.Join(listSessionDirectory(t, root, workspace), id+".jsonl")
	writeListRecords(t, path,
		`{"type":"session","version":1,"id":"`+id+`","timestamp":"2026-08-27T12:00:00Z","cwd":"`+workspace+`"}`,
	)
	return path
}

func writeCorruptJSONLSession(t *testing.T, root, workspace, id string) string {
	t.Helper()
	path := filepath.Join(listSessionDirectory(t, root, workspace), id+".jsonl")
	writeListRecords(t, path,
		`{"type":"session","version":3,"id":"`+id+`","timestamp":"2026-08-27T12:00:00Z","cwd":"`+workspace+`"}`,
		`{"type":"custom","id":"71000001","parentId":null,"timestamp":"2026-08-27T12:00:01Z","customType":"otto.runtime","data":{"profile":"default","provider":"openai-compatible","model":"test-model"}}`,
		`{"type":"message","id":"bad-json`,
	)
	return path
}

func createPiSessionAtPath(t *testing.T, path, workspace, id string) {
	t.Helper()
	writeListRecords(t, path,
		`{"type":"session","version":3,"id":"`+id+`","timestamp":"2026-08-27T12:00:00Z","cwd":"`+workspace+`"}`,
		`{"type":"custom","id":"71000001","parentId":null,"timestamp":"2026-08-27T12:00:01Z","customType":"otto.runtime","data":{"profile":"default","provider":"openai-compatible","model":"test-model"}}`,
	)
}

func writeListRecords(t *testing.T, path string, records ...string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		t.Fatal(err)
	}
	content := strings.Join(records, "\n") + "\n"
	if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}
}

func listSessionDirectory(t *testing.T, root, workspace string) string {
	t.Helper()
	key, err := workspaceKey(workspace)
	if err != nil {
		t.Fatal(err)
	}
	directory := filepath.Join(root, key)
	if err := os.MkdirAll(directory, 0o700); err != nil {
		t.Fatal(err)
	}
	return directory
}

func copyPiFixture(t *testing.T, name string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), name)
	if err := os.WriteFile(path, readFile(t, filepath.Join("testdata", "pi-v3", name)), 0o600); err != nil {
		t.Fatal(err)
	}
	return path
}

func setListMTime(t *testing.T, path string, modified time.Time) {
	t.Helper()
	if err := os.Chtimes(path, modified, modified); err != nil {
		t.Fatal(err)
	}
}

func assertSortedNewestFirst(t *testing.T, sessions []SessionInfo) {
	t.Helper()
	for i := 1; i < len(sessions); i++ {
		prev, current := sessions[i-1], sessions[i]
		if prev.Modified.Before(current.Modified) {
			t.Fatalf("sessions not sorted newest first: %#v then %#v", prev, current)
		}
		if prev.Modified.Equal(current.Modified) && prev.Path < current.Path {
			t.Fatalf("sessions not deterministically sorted for ties: %#v then %#v", prev, current)
		}
	}
}
