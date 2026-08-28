package session

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
	"time"
	"unicode/utf8"
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

func TestListStopsInspectingAfterLimitAndIgnoresOlderPoisonCandidates(t *testing.T) {
	root := t.TempDir()
	workspace := t.TempDir()
	base := time.Unix(10_000, 0).UTC()
	for index := 0; index < 20; index++ {
		path := createListSession(t, root, workspace, fmt.Sprintf("recent-%02d", index))
		setListMTime(t, path, base.Add(time.Duration(index)*time.Minute))
	}

	corrupt := writeCorruptJSONLSession(t, root, workspace, "older-corrupt-poison")
	setListMTime(t, corrupt, base.Add(-time.Hour))
	oversized := filepath.Join(listSessionDirectory(t, root, workspace), "oldest-oversize-poison.jsonl")
	file, err := os.OpenFile(oversized, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0o600)
	if err != nil {
		t.Fatal(err)
	}
	if err := file.Truncate(int64(maxSessionFileBytes) + 1); err != nil {
		_ = file.Close()
		t.Fatal(err)
	}
	if err := file.Close(); err != nil {
		t.Fatal(err)
	}
	setListMTime(t, oversized, base.Add(-2*time.Hour))

	result, err := List(context.Background(), root, workspace, "", 20)
	if err != nil {
		t.Fatal(err)
	}
	if len(result.Sessions) != 20 || result.Skipped != 0 {
		t.Fatalf("List() = %#v, want 20 recent sessions and no inspection of older poison files", result)
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

func TestListFailsWhenSessionRootIsMissing(t *testing.T) {
	root := filepath.Join(t.TempDir(), "missing-root")

	_, err := List(context.Background(), root, t.TempDir(), "", 20)
	if err == nil {
		t.Fatal("List() succeeded with a missing session root")
	}
}

func TestListRejectsSymlinkedWorkspaceDirectory(t *testing.T) {
	root := t.TempDir()
	workspace := t.TempDir()
	key, err := workspaceKey(workspace)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(t.TempDir(), filepath.Join(root, key)); err != nil {
		t.Fatal(err)
	}

	_, err = List(context.Background(), root, workspace, "", 20)
	if err == nil {
		t.Fatal("List() succeeded with a symlinked workspace directory")
	}
}

func TestOpenSessionRootNoFollowRejectsSymlink(t *testing.T) {
	root := t.TempDir()
	link := filepath.Join(t.TempDir(), "root-link")
	if err := os.Symlink(root, link); err != nil {
		t.Fatal(err)
	}

	directory, err := openSessionRootNoFollow(link)
	if err == nil {
		directory.Close()
		t.Fatal("openSessionRootNoFollow() succeeded on a symlink")
	}
}

func TestOpenWorkspaceSessionDirectoryNoFollowRejectsSymlink(t *testing.T) {
	root := t.TempDir()
	workspace := t.TempDir()
	key, err := workspaceKey(workspace)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(t.TempDir(), filepath.Join(root, key)); err != nil {
		t.Fatal(err)
	}

	rootDir, err := openSessionRootNoFollow(root)
	if err != nil {
		t.Fatal(err)
	}
	defer rootDir.Close()

	directory, _, exists, err := openWorkspaceSessionDirectoryNoFollow(rootDir, root, workspace)
	if err == nil || exists {
		if directory != nil {
			directory.Close()
		}
		t.Fatal("openWorkspaceSessionDirectoryNoFollow() succeeded on a symlink")
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

func TestInspectRejectsSymlink(t *testing.T) {
	path := copyPiFixture(t, "linear.jsonl")
	link := filepath.Join(t.TempDir(), "linked.jsonl")
	if err := os.Symlink(path, link); err != nil {
		t.Fatal(err)
	}

	_, _, err := Inspect(context.Background(), link)
	if err == nil {
		t.Fatal("Inspect() succeeded on a symlink")
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

func TestPreviewTextExactAndEllipsis(t *testing.T) {
	exact := strings.Repeat("界", maxSessionPreviewRunes)
	if got := previewText(exact); got != exact {
		t.Fatalf("previewText(exact) = %q, want %q", got, exact)
	}

	ellipsis := strings.Repeat("界", maxSessionPreviewRunes+1)
	wantEllipsis := strings.Repeat("界", maxSessionPreviewRunes) + "..."
	if got := previewText(ellipsis); got != wantEllipsis {
		t.Fatalf("previewText(ellipsis) = %q, want %q", got, wantEllipsis)
	}
}

func TestPreviewTextBoundsControlHeavyInput(t *testing.T) {
	input := strings.Repeat("\x01", 1<<20)
	runtime.GC()
	var before, after runtime.MemStats
	runtime.ReadMemStats(&before)

	got := previewText(input)

	runtime.ReadMemStats(&after)
	want := strings.Repeat(`\x01`, maxSessionPreviewRunes/4) + "..."
	if got != want {
		t.Fatalf("previewText(control-heavy) = %q, want %q", got, want)
	}
	if !utf8.ValidString(got) {
		t.Fatalf("previewText(control-heavy) returned invalid UTF-8: %q", got)
	}
	if delta := after.TotalAlloc - before.TotalAlloc; delta > 1<<20 {
		t.Fatalf("previewText(control-heavy) allocated %d bytes, want <= %d", delta, 1<<20)
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
