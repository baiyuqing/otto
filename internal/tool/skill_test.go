package tool

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/skill"
)

func writeTestSkill(t *testing.T, root, name, body string) string {
	t.Helper()
	dir := filepath.Join(root, name)
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	content := "---\nname: " + name + "\ndescription: desc for " + name + "\n---\n" + body
	if err := os.WriteFile(filepath.Join(dir, "SKILL.md"), []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
	return dir
}

func mustCatalog(t *testing.T, root string) skill.Catalog {
	t.Helper()
	catalog, warnings := skill.Discover([]string{root})
	if len(warnings) != 0 {
		t.Fatalf("unexpected warnings: %v", warnings)
	}
	return catalog
}

func TestSkillToolDefinition(t *testing.T) {
	definition := NewSkillTool(skill.Catalog{}, 51200).Definition()
	if definition.Name != "skill" {
		t.Fatalf("Name = %q, want skill", definition.Name)
	}
	params, ok := definition.Parameters["required"].([]string)
	if !ok || len(params) != 1 || params[0] != "name" {
		t.Fatalf("required = %#v, want [name]", definition.Parameters["required"])
	}
}

func TestSkillToolLoadsBodyWithHeaderAndFileList(t *testing.T) {
	root := t.TempDir()
	dir := writeTestSkill(t, root, "pdf", "# PDF handling\nBody text\n")
	if err := os.MkdirAll(filepath.Join(dir, "scripts"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "scripts", "extract.py"), []byte("x"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "references.md"), []byte("x"), 0o644); err != nil {
		t.Fatal(err)
	}
	catalog := mustCatalog(t, root)

	result := NewSkillTool(catalog, 51200).Execute(context.Background(), json.RawMessage(`{"name":"pdf"}`))
	if result.IsError {
		t.Fatalf("unexpected error: %#v", result)
	}
	if !strings.Contains(result.Content, "skill: pdf\n") {
		t.Fatalf("content missing skill header: %q", result.Content)
	}
	if !strings.Contains(result.Content, "location: "+dir+"\n") {
		t.Fatalf("content missing location header: %q", result.Content)
	}
	if !strings.Contains(result.Content, "files: references.md, scripts/extract.py\n") {
		t.Fatalf("content missing file list: %q", result.Content)
	}
	if !strings.Contains(result.Content, "# PDF handling\nBody text\n") {
		t.Fatalf("content missing body: %q", result.Content)
	}
	if strings.Contains(result.Content, "---") {
		t.Fatalf("content leaked frontmatter delimiter: %q", result.Content)
	}
	if result.PersistedContent != nil {
		t.Fatalf("PersistedContent = %#v, want nil so the body persists via Content", result.PersistedContent)
	}
}

func TestSkillToolFilesNoneWhenEmpty(t *testing.T) {
	root := t.TempDir()
	writeTestSkill(t, root, "empty", "body\n")
	catalog := mustCatalog(t, root)

	result := NewSkillTool(catalog, 51200).Execute(context.Background(), json.RawMessage(`{"name":"empty"}`))
	if result.IsError {
		t.Fatalf("unexpected error: %#v", result)
	}
	if !strings.Contains(result.Content, "files: none\n") {
		t.Fatalf("content missing files: none: %q", result.Content)
	}
}

func TestSkillToolFileCountMarkerOverFifty(t *testing.T) {
	root := t.TempDir()
	dir := writeTestSkill(t, root, "many", "body\n")
	for i := 0; i < 55; i++ {
		name := filepath.Join(dir, fmt.Sprintf("f%02d.txt", i))
		if err := os.WriteFile(name, []byte("x"), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	catalog := mustCatalog(t, root)

	result := NewSkillTool(catalog, 51200).Execute(context.Background(), json.RawMessage(`{"name":"many"}`))
	if result.IsError {
		t.Fatalf("unexpected error: %#v", result)
	}
	if !strings.Contains(result.Content, "... (55 files)") {
		t.Fatalf("content missing truncation marker: %q", result.Content)
	}
}

func TestSkillToolUnknownName(t *testing.T) {
	root := t.TempDir()
	writeTestSkill(t, root, "pdf", "body\n")
	catalog := mustCatalog(t, root)

	result := NewSkillTool(catalog, 51200).Execute(context.Background(), json.RawMessage(`{"name":"nope"}`))
	if !result.IsError || !strings.Contains(result.Content, "unknown skill: nope") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestSkillToolMissingName(t *testing.T) {
	result := NewSkillTool(skill.Catalog{}, 51200).Execute(context.Background(), json.RawMessage(`{}`))
	if !result.IsError || !strings.Contains(result.Content, "name") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestSkillToolRejectsUnknownField(t *testing.T) {
	root := t.TempDir()
	writeTestSkill(t, root, "pdf", "body\n")
	catalog := mustCatalog(t, root)

	result := NewSkillTool(catalog, 51200).Execute(context.Background(), json.RawMessage(`{"name":"pdf","extra":true}`))
	if !result.IsError || !strings.Contains(result.Content, "unknown field") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestSkillToolBodyTruncated(t *testing.T) {
	root := t.TempDir()
	writeTestSkill(t, root, "pdf", "abcdefghijklmnop\n")
	catalog := mustCatalog(t, root)

	result := NewSkillTool(catalog, 10).Execute(context.Background(), json.RawMessage(`{"name":"pdf"}`))
	if result.IsError {
		t.Fatalf("unexpected error: %#v", result)
	}
	if !strings.Contains(result.Content, "[truncated:") {
		t.Fatalf("content missing truncation marker: %q", result.Content)
	}
}

func TestSkillToolReadsFileInSubdirectory(t *testing.T) {
	root := t.TempDir()
	dir := writeTestSkill(t, root, "pdf", "body\n")
	if err := os.MkdirAll(filepath.Join(dir, "references"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "references", "api.md"), []byte("api docs\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	catalog := mustCatalog(t, root)

	result := NewSkillTool(catalog, 51200).Execute(context.Background(), json.RawMessage(`{"name":"pdf","file":"references/api.md"}`))
	if result.IsError {
		t.Fatalf("unexpected error: %#v", result)
	}
	if result.Content != "api docs\n" {
		t.Fatalf("content = %q, want %q", result.Content, "api docs\n")
	}
}

func TestSkillToolRejectsParentTraversal(t *testing.T) {
	root := t.TempDir()
	writeTestSkill(t, root, "pdf", "body\n")
	if err := os.WriteFile(filepath.Join(root, "escape.txt"), []byte("nope"), 0o644); err != nil {
		t.Fatal(err)
	}
	catalog := mustCatalog(t, root)

	result := NewSkillTool(catalog, 51200).Execute(context.Background(), json.RawMessage(`{"name":"pdf","file":"../escape.txt"}`))
	if !result.IsError || !strings.Contains(result.Content, "escapes workspace") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestSkillToolRejectsAbsolutePathOutside(t *testing.T) {
	root := t.TempDir()
	writeTestSkill(t, root, "pdf", "body\n")
	catalog := mustCatalog(t, root)

	outside := filepath.Join(t.TempDir(), "outside.txt")
	if err := os.WriteFile(outside, []byte("nope"), 0o644); err != nil {
		t.Fatal(err)
	}

	payload, err := json.Marshal(map[string]string{"name": "pdf", "file": outside})
	if err != nil {
		t.Fatal(err)
	}
	result := NewSkillTool(catalog, 51200).Execute(context.Background(), payload)
	if !result.IsError || !strings.Contains(result.Content, "escapes workspace") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestSkillToolRejectsSymlinkEscape(t *testing.T) {
	root := t.TempDir()
	dir := writeTestSkill(t, root, "pdf", "body\n")
	outside := filepath.Join(t.TempDir(), "secret.txt")
	if err := os.WriteFile(outside, []byte("secret"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(outside, filepath.Join(dir, "link.txt")); err != nil {
		t.Fatal(err)
	}
	catalog := mustCatalog(t, root)

	result := NewSkillTool(catalog, 51200).Execute(context.Background(), json.RawMessage(`{"name":"pdf","file":"link.txt"}`))
	if !result.IsError || !strings.Contains(result.Content, "escapes workspace") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestSkillToolRejectsDirectoryAsFile(t *testing.T) {
	root := t.TempDir()
	dir := writeTestSkill(t, root, "pdf", "body\n")
	if err := os.MkdirAll(filepath.Join(dir, "scripts"), 0o755); err != nil {
		t.Fatal(err)
	}
	catalog := mustCatalog(t, root)

	result := NewSkillTool(catalog, 51200).Execute(context.Background(), json.RawMessage(`{"name":"pdf","file":"scripts"}`))
	if !result.IsError || !strings.Contains(result.Content, "not a regular file") {
		t.Fatalf("unexpected result: %#v", result)
	}
}

func TestSkillToolRejectsCancelledContext(t *testing.T) {
	root := t.TempDir()
	writeTestSkill(t, root, "pdf", "body\n")
	catalog := mustCatalog(t, root)

	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	result := NewSkillTool(catalog, 51200).Execute(ctx, json.RawMessage(`{"name":"pdf"}`))
	if !result.IsError {
		t.Fatalf("unexpected result: %#v", result)
	}
}
