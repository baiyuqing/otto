package tool

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"testing"
)

func TestMatchRecursiveGlobSupportsDoubleStarAndStandardSegments(t *testing.T) {
	tests := []struct {
		pattern string
		name    string
		want    bool
	}{
		{pattern: "**/*.go", name: "main.go", want: true},
		{pattern: "**/*.go", name: "internal/tool/read.go", want: true},
		{pattern: "src/**/test*.go", name: "src/test_one.go", want: true},
		{pattern: "src/**/test*.go", name: "src/a/b/test_two.go", want: true},
		{pattern: "src/**/test*.go", name: "other/test.go", want: false},
		{pattern: "*.go", name: "main.go", want: true},
		{pattern: "*.go", name: "cmd/main.go", want: false},
		{pattern: "file[0-9].txt", name: "file7.txt", want: true},
	}
	for _, test := range tests {
		t.Run(test.pattern+"/"+test.name, func(t *testing.T) {
			got, err := matchRecursiveGlob(test.pattern, test.name)
			if err != nil {
				t.Fatal(err)
			}
			if got != test.want {
				t.Fatalf("matchRecursiveGlob(%q, %q) = %v, want %v", test.pattern, test.name, got, test.want)
			}
		})
	}
}

func TestMatchRecursiveGlobRejectsInvalidAndEscapingPatterns(t *testing.T) {
	for _, pattern := range []string{"", "/absolute/**", "../**", "src/[broken"} {
		t.Run(pattern, func(t *testing.T) {
			if _, err := matchRecursiveGlob(pattern, "src/main.go"); err == nil {
				t.Fatalf("matchRecursiveGlob(%q) error = nil, want rejection", pattern)
			}
		})
	}
}

func TestFindMatchesRecursiveGlobDeterministicallyAndSkipsGitAndSymlinks(t *testing.T) {
	root := t.TempDir()
	for _, name := range []string{"main.go", "src/test_one.go", "src/a/test_two.go", "src/skip.txt", ".hidden/test.go", ".git/secret.go"} {
		writeSearchFile(t, root, name, "content\n")
	}
	outside := t.TempDir()
	writeSearchFile(t, outside, "outside.go", "outside\n")
	if err := os.Symlink(filepath.Join(outside, "outside.go"), filepath.Join(root, "linked.go")); err != nil {
		t.Fatal(err)
	}

	find := NewFindTool(mustWorkspace(t, root), 51200)
	result := find.Execute(context.Background(), json.RawMessage(`{"pattern":"**/*.go"}`))
	if result.IsError {
		t.Fatalf("Find() error: %s", result.Content)
	}
	want := ".hidden/test.go\nmain.go\nsrc/a/test_two.go\nsrc/test_one.go\n"
	if result.Content != want {
		t.Fatalf("Find() = %q, want %q", result.Content, want)
	}

	scoped := find.Execute(context.Background(), json.RawMessage(`{"pattern":"**/test*.go","path":"src"}`))
	if scoped.IsError || scoped.Content != "src/a/test_two.go\nsrc/test_one.go\n" {
		t.Fatalf("scoped Find() = %#v", scoped)
	}
}

func TestFindEnforcesArgumentsLimitsCancellationAndWorkspace(t *testing.T) {
	root := t.TempDir()
	writeSearchFile(t, root, "a.go", "a")
	writeSearchFile(t, root, "b.go", "b")
	find := NewFindTool(mustWorkspace(t, root), 51200)

	limited := find.Execute(context.Background(), json.RawMessage(`{"pattern":"**/*.go","limit":1}`))
	if limited.IsError || !strings.HasPrefix(limited.Content, "a.go\n") || !strings.Contains(limited.Content, "result limit reached") || strings.Contains(limited.Content, "b.go") {
		t.Fatalf("limited Find() = %#v", limited)
	}

	for _, arguments := range []string{
		`{}`,
		`{"pattern":"[broken"}`,
		`{"pattern":"**","limit":0}`,
		`{"pattern":"**","limit":-1}`,
		`{"pattern":"**","limit":10001}`,
		`{"pattern":"**","extra":true}`,
		`{"pattern":"**"} true`,
		`{"pattern":"**","path":".."}`,
	} {
		result := find.Execute(context.Background(), json.RawMessage(arguments))
		if !result.IsError {
			t.Fatalf("Find(%s) = %#v, want error", arguments, result)
		}
	}

	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	canceled := find.Execute(ctx, json.RawMessage(`{"pattern":"**"}`))
	if !canceled.IsError || !strings.Contains(canceled.Content, context.Canceled.Error()) {
		t.Fatalf("canceled Find() = %#v", canceled)
	}
}

func TestFindDoesNotSearchWhenRootIsInsideGitDirectory(t *testing.T) {
	root := t.TempDir()
	writeSearchFile(t, root, ".git/config", "secret")
	find := NewFindTool(mustWorkspace(t, root), 51200)
	for _, searchPath := range []string{".git", ".git/config"} {
		result := find.Execute(context.Background(), json.RawMessage(`{"pattern":"**","path":"`+searchPath+`"}`))
		if result.IsError || result.Content != "" {
			t.Fatalf("Find(path=%q) = %#v, want empty result", searchPath, result)
		}
	}
}

func TestRecursiveSearchSkipsGitFilesAndDirectGitAliases(t *testing.T) {
	root := t.TempDir()
	writeSearchFile(t, root, ".git", "match metadata\n")
	writeSearchFile(t, root, "normal.txt", "match public\n")
	workspace := mustWorkspace(t, root)

	find := NewFindTool(workspace, 51200).Execute(context.Background(), json.RawMessage(`{"pattern":"**"}`))
	if find.IsError || find.Content != "normal.txt\n" {
		t.Fatalf("Find() = %#v, want .git metadata file skipped", find)
	}
	grep := NewGrepTool(workspace, 51200).Execute(context.Background(), json.RawMessage(`{"pattern":"match"}`))
	if grep.IsError || grep.Content != "normal.txt:1:match public\n" {
		t.Fatalf("Grep() = %#v, want .git metadata file skipped", grep)
	}

	if err := os.Remove(filepath.Join(root, ".git")); err != nil {
		t.Fatal(err)
	}
	writeSearchFile(t, root, "metadata/config", "match alias\n")
	if err := os.Symlink(filepath.Join(root, "metadata"), filepath.Join(root, ".git")); err != nil {
		t.Fatal(err)
	}
	for _, searchPath := range []string{".git", ".git/config"} {
		find := NewFindTool(workspace, 51200).Execute(context.Background(), json.RawMessage(`{"pattern":"**","path":"`+searchPath+`"}`))
		grep := NewGrepTool(workspace, 51200).Execute(context.Background(), json.RawMessage(`{"pattern":"match","path":"`+searchPath+`"}`))
		if find.IsError || find.Content != "" || grep.IsError || grep.Content != "" {
			t.Fatalf("direct alias path %q: Find=%#v Grep=%#v, want empty", searchPath, find, grep)
		}
	}
}

func TestSearchSkipsAbsoluteGitAliasThroughLexicalWorkspaceSymlink(t *testing.T) {
	parent := t.TempDir()
	realRoot := filepath.Join(parent, "real")
	if err := os.MkdirAll(realRoot, 0o755); err != nil {
		t.Fatal(err)
	}
	writeSearchFile(t, realRoot, "metadata/config", "match alias\n")
	if err := os.Symlink(filepath.Join(realRoot, "metadata"), filepath.Join(realRoot, ".git")); err != nil {
		t.Fatal(err)
	}
	aliasRoot := filepath.Join(parent, "workspace-link")
	if err := os.Symlink(realRoot, aliasRoot); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, aliasRoot)
	siblingAlias := filepath.Join(parent, "other-link")
	if err := os.Symlink(realRoot, siblingAlias); err != nil {
		t.Fatal(err)
	}
	relativeSibling, err := filepath.Rel(realRoot, filepath.Join(siblingAlias, ".git"))
	if err != nil {
		t.Fatal(err)
	}
	separator := string(filepath.Separator)
	for _, searchPath := range []string{
		filepath.Join(aliasRoot, ".git"),
		filepath.Join(realRoot, ".git"),
		filepath.Join(siblingAlias, ".git"),
		relativeSibling,
		".git" + separator + ".." + separator + "metadata",
		aliasRoot + separator + ".git" + separator + ".." + separator + "metadata",
		siblingAlias + separator + ".git" + separator + ".." + separator + "metadata",
	} {
		find := NewFindTool(workspace, 51200).Execute(context.Background(), json.RawMessage(`{"pattern":"**","path":`+strconv.Quote(searchPath)+`}`))
		grep := NewGrepTool(workspace, 51200).Execute(context.Background(), json.RawMessage(`{"pattern":"match","path":`+strconv.Quote(searchPath)+`}`))
		if find.IsError || find.Content != "" || grep.IsError || grep.Content != "" {
			t.Fatalf(".git alias path %q: Find=%#v Grep=%#v, want empty", searchPath, find, grep)
		}
	}
}

func TestSearchAllowsWorkspaceRootNamedGit(t *testing.T) {
	root := filepath.Join(t.TempDir(), ".git")
	writeSearchFile(t, root, "normal.txt", "match public\n")
	workspace := mustWorkspace(t, root)
	find := NewFindTool(workspace, 51200).Execute(context.Background(), json.RawMessage(`{"pattern":"**"}`))
	grep := NewGrepTool(workspace, 51200).Execute(context.Background(), json.RawMessage(`{"pattern":"match"}`))
	if find.IsError || find.Content != "normal.txt\n" || grep.IsError || grep.Content != "normal.txt:1:match public\n" {
		t.Fatalf("workspace named .git: Find=%#v Grep=%#v", find, grep)
	}
}

func TestSearchNormalizesPathsAndSkipsGitAliases(t *testing.T) {
	root := t.TempDir()
	writeSearchFile(t, root, "dir/file.txt", "match\n")
	writeSearchFile(t, root, ".git/config", "match secret\n")
	if err := os.Symlink(".git", filepath.Join(root, "alias")); err != nil {
		t.Fatal(err)
	}
	workspace := mustWorkspace(t, root)
	for _, tool := range []Tool{NewFindTool(workspace, 51200), NewGrepTool(workspace, 51200)} {
		arguments := `{"pattern":"**","path":"./dir/"}`
		if tool.Definition().Name == "grep" {
			arguments = `{"pattern":"match","path":"./dir/"}`
		}
		if result := tool.Execute(context.Background(), json.RawMessage(arguments)); result.IsError {
			t.Fatalf("%s ./dir/: %#v", tool.Definition().Name, result)
		}
		arguments = `{"pattern":"**","path":"alias"}`
		if tool.Definition().Name == "grep" {
			arguments = `{"pattern":"match","path":"alias"}`
		}
		if result := tool.Execute(context.Background(), json.RawMessage(arguments)); result.IsError || result.Content != "" {
			t.Fatalf("%s alias: %#v, want no git content", tool.Definition().Name, result)
		}
	}
}

func TestFindCapsOutputWithValidTruncationMarker(t *testing.T) {
	root := t.TempDir()
	writeSearchFile(t, root, "long-file-name.go", "x")
	result := NewFindTool(mustWorkspace(t, root), 8).Execute(context.Background(), json.RawMessage(`{"pattern":"**/*.go"}`))
	if result.IsError || !strings.Contains(result.Content, "truncated") || len(result.Content) > 80 {
		t.Fatalf("Find() = %#v, want bounded truncation", result)
	}
}

func TestLSListsOneLevelSortedWithTypeSuffixesAndDotfiles(t *testing.T) {
	root := t.TempDir()
	writeSearchFile(t, root, "a.txt", "a")
	writeSearchFile(t, root, ".hidden", "hidden")
	writeSearchFile(t, root, "dir/nested.txt", "nested")
	if err := os.Symlink(filepath.Join(root, "a.txt"), filepath.Join(root, "link")); err != nil {
		t.Fatal(err)
	}

	ls := NewLSTool(mustWorkspace(t, root), 51200)
	result := ls.Execute(context.Background(), json.RawMessage(`{}`))
	if result.IsError {
		t.Fatalf("LS() error: %s", result.Content)
	}
	if want := ".hidden\na.txt\ndir/\nlink@\n"; result.Content != want {
		t.Fatalf("LS() = %q, want %q", result.Content, want)
	}

	scoped := ls.Execute(context.Background(), json.RawMessage(`{"path":"dir"}`))
	if scoped.IsError || scoped.Content != "nested.txt\n" {
		t.Fatalf("scoped LS() = %#v", scoped)
	}
}

func TestLSEnforcesStrictArgumentsCancellationAndWorkspace(t *testing.T) {
	root := t.TempDir()
	writeSearchFile(t, root, "file.txt", "x")
	outside := t.TempDir()
	if err := os.Symlink(outside, filepath.Join(root, "outside")); err != nil {
		t.Fatal(err)
	}
	ls := NewLSTool(mustWorkspace(t, root), 51200)

	for _, arguments := range []string{
		`{"extra":true}`,
		`{} true`,
		`{"path":"missing"}`,
		`{"path":"file.txt"}`,
		`{"path":".."}`,
		`{"path":"outside"}`,
	} {
		result := ls.Execute(context.Background(), json.RawMessage(arguments))
		if !result.IsError {
			t.Fatalf("LS(%s) = %#v, want error", arguments, result)
		}
	}

	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	canceled := ls.Execute(ctx, json.RawMessage(`{}`))
	if !canceled.IsError || !strings.Contains(canceled.Content, context.Canceled.Error()) {
		t.Fatalf("canceled LS() = %#v", canceled)
	}
}

func TestLSCapsOutputWithValidTruncationMarker(t *testing.T) {
	root := t.TempDir()
	writeSearchFile(t, root, "long-file-name.txt", "x")
	result := NewLSTool(mustWorkspace(t, root), 8).Execute(context.Background(), json.RawMessage(`{}`))
	if result.IsError || !strings.Contains(result.Content, "truncated") || len(result.Content) > 80 {
		t.Fatalf("LS() = %#v, want bounded truncation", result)
	}
}

func TestGrepSearchesRegexWithGlobAndSkipsGitBinaryAndSymlinks(t *testing.T) {
	root := t.TempDir()
	writeSearchFile(t, root, "main.go", "TODO first\nordinary\ntodo lower\n")
	writeSearchFile(t, root, "src/a.go", "FIXME TODO nested\n")
	writeSearchFile(t, root, "src/skip.txt", "TODO text\n")
	writeSearchFile(t, root, ".hidden.go", "TODO hidden\n")
	writeSearchFile(t, root, ".git/secret.go", "TODO secret\n")
	if err := os.WriteFile(filepath.Join(root, "binary.go"), []byte{'T', 'O', 'D', 'O', 0, 'x'}, 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(root, "invalid.go"), []byte{0xff, '\n'}, 0o644); err != nil {
		t.Fatal(err)
	}
	outside := t.TempDir()
	writeSearchFile(t, outside, "outside.go", "TODO outside\n")
	if err := os.Symlink(filepath.Join(outside, "outside.go"), filepath.Join(root, "linked.go")); err != nil {
		t.Fatal(err)
	}

	grep := NewGrepTool(mustWorkspace(t, root), 51200)
	result := grep.Execute(context.Background(), json.RawMessage(`{"pattern":"TODO|FIXME","glob":"**/*.go"}`))
	if result.IsError {
		t.Fatalf("Grep() error: %s", result.Content)
	}
	want := ".hidden.go:1:TODO hidden\nmain.go:1:TODO first\nsrc/a.go:1:FIXME TODO nested\n"
	if result.Content != want {
		t.Fatalf("Grep() = %q, want %q", result.Content, want)
	}

	insensitive := grep.Execute(context.Background(), json.RawMessage(`{"pattern":"todo","path":"main.go","glob":"*.go","ignore_case":true}`))
	if insensitive.IsError || insensitive.Content != "main.go:1:TODO first\nmain.go:3:todo lower\n" {
		t.Fatalf("case-insensitive Grep() = %#v", insensitive)
	}
}

func TestGrepEnforcesArgumentsLimitsCancellationAndWorkspace(t *testing.T) {
	root := t.TempDir()
	writeSearchFile(t, root, "a.txt", "match one\nmatch two\n")
	grep := NewGrepTool(mustWorkspace(t, root), 51200)

	limited := grep.Execute(context.Background(), json.RawMessage(`{"pattern":"match","limit":1}`))
	if limited.IsError || !strings.HasPrefix(limited.Content, "a.txt:1:match one\n") || !strings.Contains(limited.Content, "result limit reached") || strings.Contains(limited.Content, "match two") {
		t.Fatalf("limited Grep() = %#v", limited)
	}

	for _, arguments := range []string{
		`{}`,
		`{"pattern":"("}`,
		`{"pattern":"match","glob":"[broken"}`,
		`{"pattern":"match","limit":0}`,
		`{"pattern":"match","limit":-1}`,
		`{"pattern":"match","limit":1001}`,
		`{"pattern":"match","extra":true}`,
		`{"pattern":"match"} true`,
		`{"pattern":"match","path":".."}`,
	} {
		result := grep.Execute(context.Background(), json.RawMessage(arguments))
		if !result.IsError {
			t.Fatalf("Grep(%s) = %#v, want error", arguments, result)
		}
	}

	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	canceled := grep.Execute(ctx, json.RawMessage(`{"pattern":"match"}`))
	if !canceled.IsError || !strings.Contains(canceled.Content, context.Canceled.Error()) {
		t.Fatalf("canceled Grep() = %#v", canceled)
	}
}

func TestGrepDoesNotSearchWhenRootIsInsideGitDirectory(t *testing.T) {
	root := t.TempDir()
	writeSearchFile(t, root, ".git/config", "match secret\n")
	grep := NewGrepTool(mustWorkspace(t, root), 51200)
	for _, searchPath := range []string{".git", ".git/config"} {
		result := grep.Execute(context.Background(), json.RawMessage(`{"pattern":"match","path":"`+searchPath+`"}`))
		if result.IsError || result.Content != "" {
			t.Fatalf("Grep(path=%q) = %#v, want empty result", searchPath, result)
		}
	}
}

func TestScanGrepReaderCancelsBetweenBoundedReads(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	reader := &cancelingReader{cancel: cancel, content: []byte("match first\nmatch second\n")}
	_, err := scanGrepReader(ctx, reader, regexp.MustCompile("match"), 100, 51200)
	if err == nil || !strings.Contains(err.Error(), context.Canceled.Error()) {
		t.Fatalf("scanGrepReader() error = %v, want cancellation", err)
	}
	if reader.reads != 1 {
		t.Fatalf("reader reads = %d, want cancellation after first bounded read", reader.reads)
	}
}

func TestScanGrepReaderSkipsFileWithOversizedLine(t *testing.T) {
	content := strings.Repeat("x", (1<<20)+1) + "\nmatch later\n"
	scan, err := scanGrepReader(context.Background(), strings.NewReader(content), regexp.MustCompile("match"), 100, 51200)
	if err != nil || scan.textFile || scan.matchOverflow || scan.byteOverflow || len(scan.matches) != 0 {
		t.Fatalf("scanGrepReader() = (%#v, %v), want skipped oversized file", scan, err)
	}
}

func TestGrepCapsOutputWithValidTruncationMarker(t *testing.T) {
	root := t.TempDir()
	writeSearchFile(t, root, "file.txt", "matching long output line\n")
	result := NewGrepTool(mustWorkspace(t, root), 8).Execute(context.Background(), json.RawMessage(`{"pattern":"matching"}`))
	if result.IsError || !strings.Contains(result.Content, "truncated") || len(result.Content) > 80 {
		t.Fatalf("Grep() = %#v, want bounded truncation", result)
	}
}

type cancelingReader struct {
	cancel  context.CancelFunc
	content []byte
	reads   int
}

func (r *cancelingReader) Read(destination []byte) (int, error) {
	r.reads++
	if r.reads > 1 {
		return 0, context.Canceled
	}
	n := copy(destination, r.content)
	r.cancel()
	return n, nil
}

func writeSearchFile(t *testing.T, root, name, content string) {
	t.Helper()
	path := filepath.Join(root, filepath.FromSlash(name))
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
}
