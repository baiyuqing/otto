package architecture

import (
	"fmt"
	"go/parser"
	"go/token"
	"io/fs"
	"os"
	"path/filepath"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"testing"
)

func TestImportRules(t *testing.T) {
	tests := []struct {
		name       string
		pkg        string
		imports    string
		wantImport string
	}{
		{name: "agent auth import is allowed", pkg: "agent", imports: `"github.com/baiyuqing/otto/internal/auth"`, wantImport: ""},
		{name: "agent cannot import concrete provider", pkg: "agent", imports: `"github.com/baiyuqing/otto/internal/provider/openaicompat"`, wantImport: "github.com/baiyuqing/otto/internal/provider/openaicompat"},
		{name: "app cannot import sqlite", pkg: "app", imports: `"github.com/baiyuqing/otto/internal/memory/sqlite"`, wantImport: "github.com/baiyuqing/otto/internal/memory/sqlite"},
		{name: "repl cannot import auth", pkg: "repl", imports: `"github.com/baiyuqing/otto/internal/auth"`, wantImport: "github.com/baiyuqing/otto/internal/auth"},
		{name: "tui cannot import tool package", pkg: "tui", imports: `"github.com/baiyuqing/otto/internal/tool"`, wantImport: "github.com/baiyuqing/otto/internal/tool"},
		{name: "tui cannot import tool implementation", pkg: "tui", imports: `"github.com/baiyuqing/otto/internal/tool/stdio"`, wantImport: "github.com/baiyuqing/otto/internal/tool/stdio"},
		{name: "server cannot import concrete provider", pkg: "server", imports: `"github.com/baiyuqing/otto/internal/provider/openairesponses"`, wantImport: "github.com/baiyuqing/otto/internal/provider/openairesponses"},
		{name: "model cannot import otto package", pkg: "model", imports: `"github.com/baiyuqing/otto/internal/app"`, wantImport: "github.com/baiyuqing/otto/internal/app"},
		{name: "skill cannot import otto package", pkg: "skill", imports: `"github.com/baiyuqing/otto/internal/model"`, wantImport: "github.com/baiyuqing/otto/internal/model"},
		{name: "standard and external imports are allowed", pkg: "model", imports: "\"encoding/json\"\n\t\"example.com/external\"", wantImport: ""},
		{name: "agent provider contract is allowed", pkg: "agent", imports: `"github.com/baiyuqing/otto/internal/provider"`, wantImport: ""},
		{name: "app memory contract is allowed", pkg: "app", imports: `"github.com/baiyuqing/otto/internal/memory"`, wantImport: ""},
		{name: "repl task contract is allowed", pkg: "repl", imports: `"github.com/baiyuqing/otto/internal/subagent"`, wantImport: ""},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			root := t.TempDir()
			dir := filepath.Join(root, "internal", tt.pkg)
			if err := os.MkdirAll(dir, 0o755); err != nil {
				t.Fatal(err)
			}
			source := "package " + tt.pkg + "\n\nimport (\n\t" + tt.imports + "\n)\n"
			if err := os.WriteFile(filepath.Join(dir, "fixture.go"), []byte(source), 0o644); err != nil {
				t.Fatal(err)
			}

			violations, err := checkRepository(root)
			if err != nil {
				t.Fatal(err)
			}
			if tt.wantImport == "" {
				if len(violations) != 0 {
					t.Fatalf("unexpected violations: %v", violations)
				}
				return
			}
			if len(violations) != 1 || violations[0].Import != tt.wantImport {
				t.Fatalf("violations = %v, want import %q", violations, tt.wantImport)
			}
			message := violations[0].Error()
			if !strings.Contains(message, "fixture.go") || !strings.Contains(message, tt.wantImport) || !strings.Contains(message, "belongs") {
				t.Fatalf("error = %q, want actionable file/import/ownership details", message)
			}
		})
	}
}

func TestImportScannerCoverage(t *testing.T) {
	root := t.TempDir()
	sources := map[string]string{
		"internal/agent/build_tagged.go":        "//go:build never\n\npackage agent\n\nimport \"github.com/baiyuqing/otto/internal/app\"\n",
		"internal/agent/nested/build_tagged.go": "//go:build never\n\npackage nested\n\nimport \"github.com/baiyuqing/otto/internal/app\"\n",
		"internal/tui/root_tool.go":             "package tui\n\nimport \"github.com/baiyuqing/otto/internal/tool\"\n",
		"internal/agent/ignored_test.go":        "package agent\n\nimport \"github.com/baiyuqing/otto/internal/app\"\n",
		"internal/agent/testdata/ignored.go":    "package agent\n\nimport \"github.com/baiyuqing/otto/internal/app\"\n",
	}
	for name, source := range sources {
		filename := filepath.Join(root, filepath.FromSlash(name))
		if err := os.MkdirAll(filepath.Dir(filename), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filename, []byte(source), 0o644); err != nil {
			t.Fatal(err)
		}
	}

	violations, err := checkRepository(root)
	if err != nil {
		t.Fatal(err)
	}
	got := make(map[string]string, len(violations))
	for _, violation := range violations {
		rel, err := filepath.Rel(root, violation.File)
		if err != nil {
			t.Fatal(err)
		}
		got[filepath.ToSlash(rel)] = violation.Import
	}
	want := map[string]string{
		"internal/agent/build_tagged.go":        modulePath + "/internal/app",
		"internal/agent/nested/build_tagged.go": modulePath + "/internal/app",
		"internal/tui/root_tool.go":             modulePath + "/internal/tool",
	}
	if len(got) != len(want) {
		t.Fatalf("violations = %v, want exactly %v", got, want)
	}
	for filename, importPath := range want {
		if got[filename] != importPath {
			t.Errorf("violation[%s] = %q, want %q", filename, got[filename], importPath)
		}
	}
}

func TestRepositoryImports(t *testing.T) {
	root, err := repositoryRoot()
	if err != nil {
		t.Fatal(err)
	}
	violations, err := checkRepository(root)
	if err != nil {
		t.Fatal(err)
	}
	if len(violations) != 0 {
		for _, violation := range violations {
			t.Error(violation)
		}
	}
}

type importViolation struct {
	File    string
	Import  string
	Belongs string
}

func (v importViolation) Error() string {
	return fmt.Sprintf("%s imports forbidden %q; dependency belongs in %s", v.File, v.Import, v.Belongs)
}

type forbiddenImport struct {
	Prefix  string
	Belongs string
}

const modulePath = "github.com/baiyuqing/otto"

func checkRepository(root string) ([]importViolation, error) {
	rules := importRules()

	var files []string
	err := fs.WalkDir(os.DirFS(root), ".", func(path string, entry fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if entry.IsDir() {
			if path != "." && strings.HasPrefix(entry.Name(), ".") {
				return fs.SkipDir
			}
			if entry.Name() == "testdata" {
				return fs.SkipDir
			}
			return nil
		}
		if strings.HasSuffix(entry.Name(), ".go") && !strings.HasSuffix(entry.Name(), "_test.go") {
			files = append(files, filepath.Join(root, filepath.FromSlash(path)))
		}
		return nil
	})
	if err != nil {
		return nil, err
	}
	sort.Strings(files)

	var violations []importViolation
	for _, filename := range files {
		target, ok := internalPackage(root, filename)
		if !ok {
			continue
		}
		forbidden := rules[target]
		if len(forbidden) == 0 {
			continue
		}
		file, err := parser.ParseFile(token.NewFileSet(), filename, nil, parser.ImportsOnly)
		if err != nil {
			return nil, fmt.Errorf("parse %s: %w", filename, err)
		}
		for _, spec := range file.Imports {
			importPath, err := strconv.Unquote(spec.Path.Value)
			if err != nil {
				return nil, fmt.Errorf("parse import in %s: %w", filename, err)
			}
			for _, rule := range forbidden {
				if (strings.HasSuffix(rule.Prefix, "/") && strings.HasPrefix(importPath, rule.Prefix)) ||
					(!strings.HasSuffix(rule.Prefix, "/") && (importPath == rule.Prefix || strings.HasPrefix(importPath, rule.Prefix+"/"))) {
					violations = append(violations, importViolation{File: filename, Import: importPath, Belongs: rule.Belongs})
					break
				}
			}
		}
	}
	sort.Slice(violations, func(i, j int) bool {
		if violations[i].File != violations[j].File {
			return violations[i].File < violations[j].File
		}
		return violations[i].Import < violations[j].Import
	})
	return violations, nil
}

func importRules() map[string][]forbiddenImport {
	anyOttoInternal := forbiddenImport{Prefix: modulePath + "/internal", Belongs: "a higher-level application or composition package"}
	concreteProvider := forbiddenImport{Prefix: modulePath + "/internal/provider/", Belongs: "the provider composition root via internal/provider"}
	memorySQLite := forbiddenImport{Prefix: modulePath + "/internal/memory/sqlite", Belongs: "the memory composition root via internal/memory"}
	auth := forbiddenImport{Prefix: modulePath + "/internal/auth", Belongs: "app.Authentication and the composition root"}
	toolImplementation := forbiddenImport{Prefix: modulePath + "/internal/tool", Belongs: "the agent/tool composition boundary"}
	return map[string][]forbiddenImport{
		"model": []forbiddenImport{anyOttoInternal},
		"skill": []forbiddenImport{anyOttoInternal},
		"agent": {
			{Prefix: modulePath + "/internal/app", Belongs: "an app-owned shared contract; keep frontend capabilities in internal/app"},
			{Prefix: modulePath + "/internal/repl", Belongs: "frontend wiring in cmd/otto"},
			{Prefix: modulePath + "/internal/tui", Belongs: "frontend wiring in cmd/otto"},
			{Prefix: modulePath + "/internal/server", Belongs: "frontend wiring in cmd/otto"},
			{Prefix: modulePath + "/internal/subagent", Belongs: "agent.New and internal/subagent construction"},
			concreteProvider,
			memorySQLite,
		},
		"app":    {concreteProvider, memorySQLite},
		"repl":   {auth, concreteProvider, memorySQLite, toolImplementation},
		"tui":    {auth, concreteProvider, memorySQLite, toolImplementation},
		"server": {auth, concreteProvider, memorySQLite, toolImplementation},
	}
}

func internalPackage(root, filename string) (string, bool) {
	rel, err := filepath.Rel(root, filename)
	if err != nil {
		return "", false
	}
	parts := strings.Split(filepath.ToSlash(filepath.Dir(rel)), "/")
	if len(parts) < 2 || parts[0] != "internal" {
		return "", false
	}
	for _, part := range parts[1:] {
		if part == "testdata" {
			return "", false
		}
	}
	return parts[1], true
}

func repositoryRoot() (string, error) {
	_, filename, _, ok := runtime.Caller(0)
	if !ok {
		return "", fmt.Errorf("locate architecture test source")
	}
	for dir := filepath.Dir(filename); ; dir = filepath.Dir(dir) {
		if _, err := os.Stat(filepath.Join(dir, "go.mod")); err == nil {
			return dir, nil
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			return "", fmt.Errorf("find go.mod from %s", filename)
		}
	}
}
