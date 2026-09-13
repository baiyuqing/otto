package tool

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
	"time"
	"unicode/utf8"
)

func TestReadRejectsInvalidJSON(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	result := NewReadTool(workspace, 51200).Execute(context.Background(), json.RawMessage(`{"path":"sample.txt"`))
	if !result.IsError || !strings.Contains(result.Content, "invalid JSON") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestReadRejectsUnknownFieldAndTrailingTokens(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "sample.txt"), []byte("hello\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	read := NewReadTool(workspace, 51200)

	unknown := read.Execute(context.Background(), json.RawMessage(`{"path":"sample.txt","extra":true}`))
	if !unknown.IsError || !strings.Contains(unknown.Content, "unknown field") {
		t.Fatalf("unexpected unknown-field result: %#v", unknown)
	}

	trailing := read.Execute(context.Background(), json.RawMessage(`{"path":"sample.txt"} true`))
	if !trailing.IsError || !strings.Contains(trailing.Content, "trailing") {
		t.Fatalf("unexpected trailing-token result: %#v", trailing)
	}
}

func TestReadRejectsMissingRequiredPath(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	result := NewReadTool(workspace, 51200).Execute(context.Background(), json.RawMessage(`{}`))
	if !result.IsError || !strings.Contains(result.Content, "path") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestReadRejectsBinaryFile(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "binary"), []byte{'a', 0, 'b'}, 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewReadTool(workspace, 51200).Execute(context.Background(), json.RawMessage(`{"path":"binary"}`))
	if !result.IsError || !strings.Contains(result.Content, "binary") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestReadRejectsInvalidUTF8(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "invalid.txt"), []byte{0xff, 0xfe}, 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewReadTool(workspace, 51200).Execute(context.Background(), json.RawMessage(`{"path":"invalid.txt"}`))
	if !result.IsError || !strings.Contains(result.Content, "UTF-8") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestReadSupportsOffsetAndLimit(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "sample.txt"), []byte("one\ntwo\nthree\nfour\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewReadTool(workspace, 51200).Execute(context.Background(), json.RawMessage(`{"path":"sample.txt","offset":2,"limit":2}`))
	if result.IsError {
		t.Fatalf("unexpected error: %#v", result)
	}
	if got, want := result.Content, "two\nthree\n"; got != want {
		t.Fatalf("content = %q, want %q", got, want)
	}
}

func TestReadReportsOutputTruncation(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "sample.txt"), []byte("abcdefghi"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewReadTool(workspace, 5).Execute(context.Background(), json.RawMessage(`{"path":"sample.txt"}`))
	if result.IsError {
		t.Fatalf("unexpected error: %#v", result)
	}
	if !strings.HasPrefix(result.Content, "abcde") || !strings.Contains(result.Content, "truncated") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestReadTruncationRemainsValidUTF8(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "sample.txt"), []byte("é"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewReadTool(workspace, 1).Execute(context.Background(), json.RawMessage(`{"path":"sample.txt"}`))
	if result.IsError {
		t.Fatalf("unexpected error: %#v", result)
	}
	if !utf8.ValidString(result.Content) {
		t.Fatalf("result content is not valid UTF-8: %q", result.Content)
	}
	if !strings.Contains(result.Content, "truncated") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestReadRejectsTraversal(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(filepath.Dir(root), "escape.txt"), []byte("nope"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewReadTool(workspace, 51200).Execute(context.Background(), json.RawMessage(`{"path":"../escape.txt"}`))
	if !result.IsError || !strings.Contains(result.Content, "escapes workspace") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestWriteRejectsInvalidJSONUnknownFieldAndTrailingTokens(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	write := NewWriteTool(workspace)

	invalid := write.Execute(context.Background(), json.RawMessage(`{"path":"file.txt","content":"x"`))
	if !invalid.IsError || !strings.Contains(invalid.Content, "invalid JSON") {
		t.Fatalf("unexpected invalid-json result: %#v", invalid)
	}

	unknown := write.Execute(context.Background(), json.RawMessage(`{"path":"file.txt","content":"x","extra":true}`))
	if !unknown.IsError || !strings.Contains(unknown.Content, "unknown field") {
		t.Fatalf("unexpected unknown-field result: %#v", unknown)
	}

	trailing := write.Execute(context.Background(), json.RawMessage(`{"path":"file.txt","content":"x"} []`))
	if !trailing.IsError || !strings.Contains(trailing.Content, "trailing") {
		t.Fatalf("unexpected trailing-token result: %#v", trailing)
	}
}

func TestWriteRejectsMissingRequiredPath(t *testing.T) {
	workspace := mustWorkspace(t, t.TempDir())
	result := NewWriteTool(workspace).Execute(context.Background(), json.RawMessage(`{"content":"hello"}`))
	if !result.IsError || !strings.Contains(result.Content, "path") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestWriteIsAtomicAndCreatesParents(t *testing.T) {
	root := t.TempDir()
	workspace := mustWorkspace(t, root)
	result := NewWriteTool(workspace).Execute(context.Background(), json.RawMessage(`{"path":"nested/file.txt","content":"hello"}`))
	if result.IsError {
		t.Fatal(result.Content)
	}
	data, err := os.ReadFile(filepath.Join(root, "nested/file.txt"))
	if err != nil || string(data) != "hello" {
		t.Fatalf("data=%q err=%v", data, err)
	}
}

func TestWritePreservesExistingPermissions(t *testing.T) {
	root := t.TempDir()
	path := filepath.Join(root, "file.txt")
	if err := os.WriteFile(path, []byte("before"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(path, 0o600); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewWriteTool(workspace).Execute(context.Background(), json.RawMessage(`{"path":"file.txt","content":"after"}`))
	if result.IsError {
		t.Fatal(result.Content)
	}
	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	if got := info.Mode().Perm(); got != 0o600 {
		t.Fatalf("mode = %#o, want %#o", got, 0o600)
	}
}

func TestWriteLeavesNoTemporaryFilesAfterSuccess(t *testing.T) {
	root := t.TempDir()
	workspace := mustWorkspace(t, root)
	result := NewWriteTool(workspace).Execute(context.Background(), json.RawMessage(`{"path":"file.txt","content":"hello"}`))
	if result.IsError {
		t.Fatal(result.Content)
	}
	entries, err := os.ReadDir(root)
	if err != nil {
		t.Fatal(err)
	}
	if len(entries) != 1 || entries[0].Name() != "file.txt" {
		t.Fatalf("unexpected directory entries: %v", entryNames(entries))
	}
}

func TestWriteRejectsSymlinkEscape(t *testing.T) {
	root := t.TempDir()
	outside := t.TempDir()
	if err := os.Symlink(outside, filepath.Join(root, "link")); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewWriteTool(workspace).Execute(context.Background(), json.RawMessage(`{"path":"link/file.txt","content":"hello"}`))
	if !result.IsError || !strings.Contains(result.Content, "escapes workspace") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestEditRejectsInvalidJSONUnknownFieldAndTrailingTokens(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "sample.txt"), []byte("same\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	edit := NewEditTool(workspace)

	invalid := edit.Execute(context.Background(), json.RawMessage(`{"path":"sample.txt","old_text":"same","new_text":"new"`))
	if !invalid.IsError || !strings.Contains(invalid.Content, "invalid JSON") {
		t.Fatalf("unexpected invalid-json result: %#v", invalid)
	}

	unknown := edit.Execute(context.Background(), json.RawMessage(`{"path":"sample.txt","old_text":"same","new_text":"new","extra":true}`))
	if !unknown.IsError || !strings.Contains(unknown.Content, "unknown field") {
		t.Fatalf("unexpected unknown-field result: %#v", unknown)
	}

	trailing := edit.Execute(context.Background(), json.RawMessage(`{"path":"sample.txt","old_text":"same","new_text":"new"} 1`))
	if !trailing.IsError || !strings.Contains(trailing.Content, "trailing") {
		t.Fatalf("unexpected trailing-token result: %#v", trailing)
	}
}

func TestEditRejectsMissingRequiredArguments(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "sample.txt"), []byte("same\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)

	missingPath := NewEditTool(workspace).Execute(context.Background(), json.RawMessage(`{"old_text":"same","new_text":"new"}`))
	if !missingPath.IsError || !strings.Contains(missingPath.Content, "path") {
		t.Fatalf("unexpected missing-path result: %#v", missingPath)
	}

	missingOldText := NewEditTool(workspace).Execute(context.Background(), json.RawMessage(`{"path":"sample.txt","new_text":"new"}`))
	if !missingOldText.IsError || !strings.Contains(missingOldText.Content, "old_text") {
		t.Fatalf("unexpected missing-old_text result: %#v", missingOldText)
	}
}

func TestEditRejectsAmbiguousMatch(t *testing.T) {
	root := t.TempDir()
	path := filepath.Join(root, "sample.txt")
	if err := os.WriteFile(path, []byte("same\nsame\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	edit := NewEditTool(workspace)
	result := edit.Execute(context.Background(), json.RawMessage(`{"path":"sample.txt","old_text":"same","new_text":"new"}`))
	want := "edit failed: old_text matched 2 locations in sample.txt; include more surrounding context to make it unique"
	if !result.IsError || result.Content != want {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestEditRejectsAbsentMatch(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "sample.txt"), []byte("same\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewEditTool(workspace).Execute(context.Background(), json.RawMessage(`{"path":"sample.txt","old_text":"missing","new_text":"new"}`))
	want := "edit failed: old_text was not found in sample.txt"
	if !result.IsError || result.Content != want {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestEditErrorsDoNotEchoLargeOldText(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "sample.txt"), []byte("dup\ndup\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	edit := NewEditTool(workspace)

	large, err := json.Marshal(map[string]string{
		"path":     "sample.txt",
		"old_text": strings.Repeat("ZQX marker ", 2000),
		"new_text": "new",
	})
	if err != nil {
		t.Fatal(err)
	}
	notFound := edit.Execute(context.Background(), json.RawMessage(large))
	if !notFound.IsError || strings.Contains(notFound.Content, "ZQX") || len(notFound.Content) > 200 {
		t.Fatalf("not-found error echoes old_text or is oversized: len=%d %#v", len(notFound.Content), notFound)
	}

	ambiguous := edit.Execute(context.Background(), json.RawMessage(`{"path":"sample.txt","old_text":"dup","new_text":"new"}`))
	if !ambiguous.IsError || strings.Contains(ambiguous.Content, "dup") || len(ambiguous.Content) > 200 {
		t.Fatalf("ambiguous error echoes old_text or is oversized: len=%d %#v", len(ambiguous.Content), ambiguous)
	}
}

func TestEditRejectsBinaryFile(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "binary"), []byte{'a', 0, 'b'}, 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewEditTool(workspace).Execute(context.Background(), json.RawMessage(`{"path":"binary","old_text":"a","new_text":"c"}`))
	if !result.IsError || !strings.Contains(result.Content, "binary") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestEditSucceedsWithExactSingleMatch(t *testing.T) {
	root := t.TempDir()
	path := filepath.Join(root, "sample.txt")
	if err := os.WriteFile(path, []byte("hello world\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewEditTool(workspace).Execute(context.Background(), json.RawMessage(`{"path":"sample.txt","old_text":"world","new_text":"there"}`))
	if result.IsError {
		t.Fatalf("unexpected error: %#v", result)
	}
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if got, want := string(data), "hello there\n"; got != want {
		t.Fatalf("content = %q, want %q", got, want)
	}
	if !strings.Contains(result.Content, "sample.txt") {
		t.Fatalf("result does not name the file: %#v", result)
	}
	if !strings.Contains(result.Content, "-hello world") || !strings.Contains(result.Content, "+hello there") {
		t.Fatalf("result lacks a diff of the change: %#v", result)
	}
}

func TestEditAcceptsMultiEditArguments(t *testing.T) {
	root := t.TempDir()
	path := filepath.Join(root, "sample.txt")
	if err := os.WriteFile(path, []byte("one\ntwo\nthree\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewEditTool(workspace).Execute(context.Background(), json.RawMessage(`{
		"path":"sample.txt",
		"edits":[
			{"old_text":"one","new_text":"ONE"},
			{"old_text":"three","new_text":"THREE"}
		]
	}`))
	if result.IsError {
		t.Fatalf("unexpected error: %#v", result)
	}
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if got, want := string(data), "ONE\ntwo\nTHREE\n"; got != want {
		t.Fatalf("content = %q, want %q", got, want)
	}
}

func TestEditRejectsUntypedEditShapes(t *testing.T) {
	root := t.TempDir()
	path := filepath.Join(root, "sample.txt")
	if err := os.WriteFile(path, []byte("a b c\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	edit := NewEditTool(workspace)

	cases := map[string]string{
		"camel case keys":       `{"path":"sample.txt","oldText":"a","newText":"A"}`,
		"string encoded edits":  `{"path":"sample.txt","edits":"[{\"old_text\":\"a\",\"new_text\":\"A\"}]"}`,
		"object edits":          `{"path":"sample.txt","edits":{"old_text":"a","new_text":"A"}}`,
		"single and list edits": `{"path":"sample.txt","old_text":"a","new_text":"A","edits":[{"old_text":"c","new_text":"C"}]}`,
		"empty edits":           `{"path":"sample.txt","edits":[]}`,
		"edit without new_text": `{"path":"sample.txt","edits":[{"old_text":"a"}]}`,
	}
	for name, arguments := range cases {
		result := edit.Execute(context.Background(), json.RawMessage(arguments))
		if !result.IsError {
			t.Fatalf("%s: expected an error, got %#v", name, result)
		}
	}
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if got, want := string(data), "a b c\n"; got != want {
		t.Fatalf("content = %q, want %q", got, want)
	}
}

func TestEditAllowsNullEditsWithSingleReplacement(t *testing.T) {
	root := t.TempDir()
	path := filepath.Join(root, "sample.txt")
	if err := os.WriteFile(path, []byte("a\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewEditTool(workspace).Execute(context.Background(), json.RawMessage(`{"path":"sample.txt","old_text":"a","new_text":"A","edits":null}`))
	if result.IsError {
		t.Fatalf("unexpected error: %#v", result)
	}
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if got, want := string(data), "A\n"; got != want {
		t.Fatalf("content = %q, want %q", got, want)
	}
}

func TestEditFuzzyMatchesWhitespaceQuotesAndDashes(t *testing.T) {
	root := t.TempDir()
	path := filepath.Join(root, "sample.txt")
	if err := os.WriteFile(path, []byte("const msg = “hello”  \nconst dash = \"a—b\"\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewEditTool(workspace).Execute(context.Background(), json.RawMessage(`{
		"path":"sample.txt",
		"old_text":"const msg = \"hello\"\nconst dash = \"a-b\"",
		"new_text":"const msg = \"hi\"\nconst dash = \"a-b\""
	}`))
	if result.IsError {
		t.Fatalf("unexpected error: %#v", result)
	}
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if got, want := string(data), "const msg = “hi”  \nconst dash = \"a—b\"\n"; got != want {
		t.Fatalf("content = %q, want %q", got, want)
	}
}

func TestEditFuzzyMatchOnlyRewritesChangedSpan(t *testing.T) {
	cases := []struct {
		name, file, oldText, newText, want string
	}{
		{"keeps markdown hard break", "line one  \nline two\n", "line one\nline two", "line one\nline 2", "line one  \nline 2\n"},
		{"keeps curly quotes and em dash", "x = “a” — b\n", "x = \"a\" - b", "x = \"a\" - c", "x = “a” — c\n"},
		{"keeps trailing whitespace after changed word", "foo  \nbar\n", "foo\nbar", "baz\nbar", "baz  \nbar\n"},
		{"keeps next line indentation", "foo  \n    bar\n", "foo\n    ", "FOO\n    ", "FOO  \n    bar\n"},
		{"inserts between fuzzy lines", "a  \nb\n", "a\nb", "a\nX\nb", "a  \nX\nb\n"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			root := t.TempDir()
			path := filepath.Join(root, "sample.txt")
			if err := os.WriteFile(path, []byte(tc.file), 0o644); err != nil {
				t.Fatal(err)
			}
			arguments, err := json.Marshal(map[string]string{"path": "sample.txt", "old_text": tc.oldText, "new_text": tc.newText})
			if err != nil {
				t.Fatal(err)
			}
			result := NewEditTool(mustWorkspace(t, root)).Execute(context.Background(), json.RawMessage(arguments))
			if result.IsError {
				t.Fatalf("unexpected error: %#v", result)
			}
			data, err := os.ReadFile(path)
			if err != nil {
				t.Fatal(err)
			}
			if got := string(data); got != tc.want {
				t.Fatalf("content = %q, want %q", got, tc.want)
			}
		})
	}
}

func TestEditRejectsWhitespaceOnlyFuzzyOldText(t *testing.T) {
	root := t.TempDir()
	path := filepath.Join(root, "sample.txt")
	if err := os.WriteFile(path, []byte("abc  \n"), 0o644); err != nil {
		t.Fatal(err)
	}
	result := NewEditTool(mustWorkspace(t, root)).Execute(context.Background(), json.RawMessage(`{"path":"sample.txt","old_text":"\t\n","new_text":"X"}`))
	if !result.IsError || !strings.Contains(result.Content, "old_text was not found") {
		t.Fatalf("unexpected result: %#v", result)
	}
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if got, want := string(data), "abc  \n"; got != want {
		t.Fatalf("content = %q, want %q", got, want)
	}
}

func TestEditMatchesBOMPrefixedOldText(t *testing.T) {
	for _, newText := range []string{"bye", "\ufeffbye"} {
		root := t.TempDir()
		path := filepath.Join(root, "sample.txt")
		if err := os.WriteFile(path, []byte("\xef\xbb\xbfhello\nworld\n"), 0o644); err != nil {
			t.Fatal(err)
		}
		arguments, err := json.Marshal(map[string]string{"path": "sample.txt", "old_text": "\ufeffhello", "new_text": newText})
		if err != nil {
			t.Fatal(err)
		}
		result := NewEditTool(mustWorkspace(t, root)).Execute(context.Background(), json.RawMessage(arguments))
		if result.IsError {
			t.Fatalf("new_text %q: unexpected error: %#v", newText, result)
		}
		data, err := os.ReadFile(path)
		if err != nil {
			t.Fatal(err)
		}
		if got, want := string(data), "\xef\xbb\xbfbye\nworld\n"; got != want {
			t.Fatalf("new_text %q: content = %q, want %q", newText, got, want)
		}
	}
}

func TestEditKeepsLFWhenFileContainsStrayCR(t *testing.T) {
	root := t.TempDir()
	path := filepath.Join(root, "sample.txt")
	if err := os.WriteFile(path, []byte("a\rb\nc\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	result := NewEditTool(mustWorkspace(t, root)).Execute(context.Background(), json.RawMessage(`{"path":"sample.txt","old_text":"c","new_text":"x\ny"}`))
	if result.IsError {
		t.Fatalf("unexpected error: %#v", result)
	}
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if got, want := string(data), "a\rb\nx\ny\n"; got != want {
		t.Fatalf("content = %q, want %q", got, want)
	}
}

func TestEditDiffRendersOneHunkPerEdit(t *testing.T) {
	root := t.TempDir()
	var lines []string
	for i := 1; i <= 200; i++ {
		lines = append(lines, "line "+strconv.Itoa(i))
	}
	if err := os.WriteFile(filepath.Join(root, "sample.txt"), []byte(strings.Join(lines, "\n")+"\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewEditTool(workspace).Execute(context.Background(), json.RawMessage(`{
		"path":"sample.txt",
		"edits":[
			{"old_text":"line 5\n","new_text":"line 5\nextra\n"},
			{"old_text":"line 200\n","new_text":"LAST\n"}
		]
	}`))
	if result.IsError {
		t.Fatalf("unexpected error: %#v", result)
	}
	for _, want := range []string{"@@ -3,6 +3,7 @@", "+extra", "@@ -197,5 +198,5 @@", "-line 200", "+LAST"} {
		if !strings.Contains(result.Content, want) {
			t.Fatalf("diff missing %q: %#v", want, result)
		}
	}
	for _, absent := range []string{"line 100", "truncated", "-line 6"} {
		if strings.Contains(result.Content, absent) {
			t.Fatalf("diff includes %q: %#v", absent, result)
		}
	}
}

func TestEditDiffMergesEditsOnOneLine(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "sample.txt"), []byte("a b c\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	result := NewEditTool(mustWorkspace(t, root)).Execute(context.Background(), json.RawMessage(`{
		"path":"sample.txt",
		"edits":[
			{"old_text":"a","new_text":"A"},
			{"old_text":"c","new_text":"C"}
		]
	}`))
	if result.IsError {
		t.Fatalf("unexpected error: %#v", result)
	}
	if strings.Count(result.Content, "-a b c") != 1 || strings.Count(result.Content, "+A b C") != 1 || strings.Count(result.Content, "@@") != 2 {
		t.Fatalf("unexpected diff: %#v", result)
	}
}

func TestWriteAndEditShareFileLock(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "sample.txt"), []byte("a\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	key, err := workspace.writeRelative("sample.txt")
	if err != nil {
		t.Fatal(err)
	}
	calls := map[string]func() Result{
		"write": func() Result {
			return NewWriteTool(workspace).Execute(context.Background(), json.RawMessage(`{"path":"sample.txt","content":"b\n"}`))
		},
		"edit": func() Result {
			return NewEditTool(workspace).Execute(context.Background(), json.RawMessage(`{"path":"sample.txt","old_text":"a","new_text":"b"}`))
		},
	}
	for name, call := range calls {
		unlock := workspace.lockPath(key)
		done := make(chan Result, 1)
		go func() { done <- call() }()
		select {
		case result := <-done:
			t.Fatalf("%s completed while the file lock was held: %#v", name, result)
		case <-time.After(50 * time.Millisecond):
		}
		unlock()
		if result := <-done; result.IsError {
			t.Fatalf("%s: unexpected error: %#v", name, result)
		}
		if err := os.WriteFile(filepath.Join(root, "sample.txt"), []byte("a\n"), 0o644); err != nil {
			t.Fatal(err)
		}
	}
}

func TestEditPreservesBOMAndCRLF(t *testing.T) {
	root := t.TempDir()
	path := filepath.Join(root, "sample.txt")
	if err := os.WriteFile(path, []byte("\xef\xbb\xbfa\r\nb\r\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewEditTool(workspace).Execute(context.Background(), json.RawMessage(`{"path":"sample.txt","old_text":"a\nb\n","new_text":"x\ny\n"}`))
	if result.IsError {
		t.Fatalf("unexpected error: %#v", result)
	}
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if got, want := string(data), "\xef\xbb\xbfx\r\ny\r\n"; got != want {
		t.Fatalf("content = %q, want %q", got, want)
	}
}

func TestEditRejectsOverlappingMultiEdits(t *testing.T) {
	root := t.TempDir()
	path := filepath.Join(root, "sample.txt")
	if err := os.WriteFile(path, []byte("abcdef\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewEditTool(workspace).Execute(context.Background(), json.RawMessage(`{
		"path":"sample.txt",
		"edits":[
			{"old_text":"abc","new_text":"ABC"},
			{"old_text":"bcd","new_text":"BCD"}
		]
	}`))
	if !result.IsError || !strings.Contains(result.Content, "overlap") {
		t.Fatalf("unexpected result: %#v", result)
	}
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if got, want := string(data), "abcdef\n"; got != want {
		t.Fatalf("content = %q, want %q", got, want)
	}
}

func TestEditMatchesAllEditsAgainstOriginalContent(t *testing.T) {
	root := t.TempDir()
	path := filepath.Join(root, "sample.txt")
	if err := os.WriteFile(path, []byte("a\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewEditTool(workspace).Execute(context.Background(), json.RawMessage(`{
		"path":"sample.txt",
		"edits":[
			{"old_text":"a","new_text":"b"},
			{"old_text":"b","new_text":"c"}
		]
	}`))
	if !result.IsError || !strings.Contains(result.Content, "old_text was not found") {
		t.Fatalf("unexpected result: %#v", result)
	}
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if got, want := string(data), "a\n"; got != want {
		t.Fatalf("content = %q, want %q", got, want)
	}
}

func TestEditResultDiffIsCompactWithContext(t *testing.T) {
	root := t.TempDir()
	var lines []string
	for i := 1; i <= 40; i++ {
		lines = append(lines, "line "+strconv.Itoa(i))
	}
	content := strings.Join(lines, "\n") + "\n"
	if err := os.WriteFile(filepath.Join(root, "sample.txt"), []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewEditTool(workspace).Execute(context.Background(), json.RawMessage(`{"path":"sample.txt","old_text":"line 20\n","new_text":"changed 20\n"}`))
	if result.IsError {
		t.Fatalf("unexpected error: %#v", result)
	}
	for _, want := range []string{"@@ -17,7 +17,7 @@", "-line 20", "+changed 20", " line 17", " line 23"} {
		if !strings.Contains(result.Content, want) {
			t.Fatalf("diff missing %q: %#v", want, result)
		}
	}
	for _, absent := range []string{"line 13", "line 27", "line 40"} {
		if strings.Contains(result.Content, absent) {
			t.Fatalf("diff includes far-away line %q: %#v", absent, result)
		}
	}
}

func TestEditDiffTrimsUnchangedLinesInsideOldText(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "sample.txt"), []byte("alpha\nbeta\ngamma\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewEditTool(workspace).Execute(context.Background(), json.RawMessage(`{"path":"sample.txt","old_text":"alpha\nbeta\ngamma\n","new_text":"alpha\nCHANGED\ngamma\n"}`))
	if result.IsError {
		t.Fatalf("unexpected error: %#v", result)
	}
	if !strings.Contains(result.Content, "-beta") || !strings.Contains(result.Content, "+CHANGED") {
		t.Fatalf("diff missing changed line: %#v", result)
	}
	if strings.Contains(result.Content, "-alpha") || strings.Contains(result.Content, "-gamma") {
		t.Fatalf("diff marks unchanged lines as removed: %#v", result)
	}
}

func TestEditTruncatesOversizedDiff(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "sample.txt"), []byte("target\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	arguments, err := json.Marshal(map[string]string{
		"path":     "sample.txt",
		"old_text": "target\n",
		"new_text": strings.Repeat("replacement line\n", 2000),
	})
	if err != nil {
		t.Fatal(err)
	}
	result := NewEditTool(workspace).Execute(context.Background(), json.RawMessage(arguments))
	if result.IsError {
		t.Fatalf("unexpected error: %#v", result)
	}
	if !strings.Contains(result.Content, "truncated") || len(result.Content) > 8192 {
		t.Fatalf("diff not truncated: len=%d", len(result.Content))
	}
	data, err := os.ReadFile(filepath.Join(root, "sample.txt"))
	if err != nil {
		t.Fatal(err)
	}
	if got, want := len(data), len("replacement line\n")*2000; got != want {
		t.Fatalf("file length = %d, want %d", got, want)
	}
}

func TestEditRejectsTraversal(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(filepath.Dir(root), "escape.txt"), []byte("x"), 0o644); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	result := NewEditTool(workspace).Execute(context.Background(), json.RawMessage(`{"path":"../escape.txt","old_text":"x","new_text":"y"}`))
	if !result.IsError || !strings.Contains(result.Content, "escapes workspace") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func mustWorkspace(t *testing.T, root string) *Workspace {
	t.Helper()
	workspace, err := NewWorkspace(root)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = workspace.Close() })
	return workspace
}

func entryNames(entries []os.DirEntry) []string {
	names := make([]string, 0, len(entries))
	for _, entry := range entries {
		names = append(names, entry.Name())
	}
	return names
}
