package subagent

import (
	"os"
	"path/filepath"
	"strings"
	"syscall"
	"testing"
	"time"
)

func writeAgent(t *testing.T, root, name, frontmatterExtra, body string) string {
	t.Helper()
	dir := filepath.Join(root, name)
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	content := "---\nname: " + name + "\ndescription: desc for " + name + "\n" + frontmatterExtra + "---\n" + body
	if err := os.WriteFile(filepath.Join(dir, "AGENT.md"), []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
	return dir
}

func TestDiscoverValidDefinitionAllFields(t *testing.T) {
	root := t.TempDir()
	dir := writeAgent(t, root, "reviewer",
		"tools: read, grep ,bash\nmodel: gpt-4o-mini\ncontext: inherit\n",
		"Review the diff.\n")

	catalog, warnings := Discover([]string{root})
	if len(warnings) != 0 {
		t.Fatalf("warnings = %v, want none", warnings)
	}
	if catalog.Len() != 1 {
		t.Fatalf("Len() = %d, want 1", catalog.Len())
	}
	def, ok := catalog.Lookup("reviewer")
	if !ok {
		t.Fatalf("reviewer definition not found")
	}
	if def.Name != "reviewer" {
		t.Fatalf("Name = %q", def.Name)
	}
	if def.Description != "desc for reviewer" {
		t.Fatalf("Description = %q", def.Description)
	}
	wantTools := []string{"read", "grep", "bash"}
	if len(def.Tools) != len(wantTools) {
		t.Fatalf("Tools = %#v, want %#v", def.Tools, wantTools)
	}
	for i := range wantTools {
		if def.Tools[i] != wantTools[i] {
			t.Fatalf("Tools = %#v, want %#v", def.Tools, wantTools)
		}
	}
	if def.Model != "gpt-4o-mini" {
		t.Fatalf("Model = %q", def.Model)
	}
	if def.Context != "inherit" {
		t.Fatalf("Context = %q", def.Context)
	}
	if def.Body != "Review the diff." {
		t.Fatalf("Body = %q", def.Body)
	}
	if def.Dir != dir {
		t.Fatalf("Dir = %q, want %q", def.Dir, dir)
	}
	if def.Path != filepath.Join(dir, "AGENT.md") {
		t.Fatalf("Path = %q", def.Path)
	}
}

func TestDiscoverMissingName(t *testing.T) {
	root := t.TempDir()
	dir := filepath.Join(root, "noname")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	content := "---\ndescription: d\n---\nbody\n"
	if err := os.WriteFile(filepath.Join(dir, "AGENT.md"), []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
	catalog, warnings := Discover([]string{root})
	if catalog.Len() != 0 {
		t.Fatalf("Len() = %d, want 0", catalog.Len())
	}
	if len(warnings) != 1 || !strings.Contains(warnings[0], "missing name") {
		t.Fatalf("warnings = %v, want one containing missing name", warnings)
	}
}

func TestDiscoverNameDirMismatch(t *testing.T) {
	root := t.TempDir()
	dir := filepath.Join(root, "actualdir")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	content := "---\nname: otherdir\ndescription: d\n---\nbody\n"
	if err := os.WriteFile(filepath.Join(dir, "AGENT.md"), []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
	catalog, warnings := Discover([]string{root})
	if catalog.Len() != 0 {
		t.Fatalf("Len() = %d, want 0", catalog.Len())
	}
	if len(warnings) != 1 || !strings.Contains(warnings[0], "does not match directory") {
		t.Fatalf("warnings = %v, want one containing does not match directory", warnings)
	}
}

func TestDiscoverInvalidNameChars(t *testing.T) {
	root := t.TempDir()
	dir := filepath.Join(root, "Upper")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	content := "---\nname: Upper\ndescription: d\n---\nbody\n"
	if err := os.WriteFile(filepath.Join(dir, "AGENT.md"), []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
	catalog, warnings := Discover([]string{root})
	if catalog.Len() != 0 {
		t.Fatalf("Len() = %d, want 0", catalog.Len())
	}
	if len(warnings) != 1 || !strings.Contains(warnings[0], "invalid") {
		t.Fatalf("warnings = %v, want one containing invalid", warnings)
	}
}

func TestDiscoverMissingDescription(t *testing.T) {
	root := t.TempDir()
	dir := filepath.Join(root, "x")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	content := "---\nname: x\n---\nbody\n"
	if err := os.WriteFile(filepath.Join(dir, "AGENT.md"), []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
	catalog, warnings := Discover([]string{root})
	if catalog.Len() != 0 {
		t.Fatalf("Len() = %d, want 0", catalog.Len())
	}
	if len(warnings) != 1 || !strings.Contains(warnings[0], "missing description") {
		t.Fatalf("warnings = %v, want one containing missing description", warnings)
	}
}

func TestDiscoverDescriptionTooLong(t *testing.T) {
	root := t.TempDir()
	dir := filepath.Join(root, "x")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	long := strings.Repeat("a", 1025)
	content := "---\nname: x\ndescription: " + long + "\n---\nbody\n"
	if err := os.WriteFile(filepath.Join(dir, "AGENT.md"), []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
	catalog, warnings := Discover([]string{root})
	if catalog.Len() != 0 {
		t.Fatalf("Len() = %d, want 0", catalog.Len())
	}
	if len(warnings) != 1 || !strings.Contains(warnings[0], "exceeds 1024") {
		t.Fatalf("warnings = %v, want one containing exceeds 1024", warnings)
	}
}

func TestDiscoverToolsParsedWithSpaces(t *testing.T) {
	root := t.TempDir()
	writeAgent(t, root, "x", "tools: read, grep ,bash\n", "body\n")
	catalog, warnings := Discover([]string{root})
	if len(warnings) != 0 {
		t.Fatalf("warnings = %v, want none", warnings)
	}
	def, ok := catalog.Lookup("x")
	if !ok {
		t.Fatalf("definition not found")
	}
	want := []string{"read", "grep", "bash"}
	if len(def.Tools) != len(want) {
		t.Fatalf("Tools = %#v, want %#v", def.Tools, want)
	}
	for i := range want {
		if def.Tools[i] != want[i] {
			t.Fatalf("Tools = %#v, want %#v", def.Tools, want)
		}
	}
}

func TestDiscoverToolsEmptyValueWarnsAndSkips(t *testing.T) {
	root := t.TempDir()
	writeAgent(t, root, "x", "tools: \n", "body\n")
	catalog, warnings := Discover([]string{root})
	if catalog.Len() != 0 {
		t.Fatalf("Len() = %d, want 0", catalog.Len())
	}
	if len(warnings) != 1 || !strings.Contains(warnings[0], "tools must be a comma-separated list") {
		t.Fatalf("warnings = %v, want one containing tools must be a comma-separated list", warnings)
	}
}

func TestDiscoverToolsOnlyCommasWarnsAndSkips(t *testing.T) {
	root := t.TempDir()
	writeAgent(t, root, "x", "tools: \" , , \"\n", "body\n")
	catalog, warnings := Discover([]string{root})
	if catalog.Len() != 0 {
		t.Fatalf("Len() = %d, want 0", catalog.Len())
	}
	if len(warnings) != 1 || !strings.Contains(warnings[0], "tools must be a comma-separated list") {
		t.Fatalf("warnings = %v, want one containing tools must be a comma-separated list", warnings)
	}
}

func TestDiscoverToolsInvalidItemSkipsWithWarning(t *testing.T) {
	root := t.TempDir()
	writeAgent(t, root, "x", "tools: Read, bash\n", "body\n")
	catalog, warnings := Discover([]string{root})
	if catalog.Len() != 0 {
		t.Fatalf("Len() = %d, want 0", catalog.Len())
	}
	if len(warnings) != 1 {
		t.Fatalf("warnings = %v, want one warning", warnings)
	}
}

func TestDiscoverContextInheritAccepted(t *testing.T) {
	root := t.TempDir()
	writeAgent(t, root, "x", "context: inherit\n", "body\n")
	catalog, warnings := Discover([]string{root})
	if len(warnings) != 0 {
		t.Fatalf("warnings = %v, want none", warnings)
	}
	def, ok := catalog.Lookup("x")
	if !ok || def.Context != "inherit" {
		t.Fatalf("Context = %q, want inherit (ok=%v)", def.Context, ok)
	}
}

func TestDiscoverContextInvalidWarnsAndSkips(t *testing.T) {
	root := t.TempDir()
	writeAgent(t, root, "x", "context: foo\n", "body\n")
	catalog, warnings := Discover([]string{root})
	if catalog.Len() != 0 {
		t.Fatalf("Len() = %d, want 0", catalog.Len())
	}
	if len(warnings) != 1 || !strings.Contains(warnings[0], `context must be "fresh" or "inherit"`) {
		t.Fatalf("warnings = %v, want one containing context must be \"fresh\" or \"inherit\"", warnings)
	}
}

func TestDiscoverContextAbsentDefaultsToFresh(t *testing.T) {
	root := t.TempDir()
	writeAgent(t, root, "x", "", "body\n")
	catalog, warnings := Discover([]string{root})
	if len(warnings) != 0 {
		t.Fatalf("warnings = %v, want none", warnings)
	}
	def, ok := catalog.Lookup("x")
	if !ok || def.Context != "fresh" {
		t.Fatalf("Context = %q, want fresh (ok=%v)", def.Context, ok)
	}
}

func TestDiscoverModelAbsentIsEmpty(t *testing.T) {
	root := t.TempDir()
	writeAgent(t, root, "x", "", "body\n")
	catalog, _ := Discover([]string{root})
	def, ok := catalog.Lookup("x")
	if !ok || def.Model != "" {
		t.Fatalf("Model = %q, want empty (ok=%v)", def.Model, ok)
	}
}

func TestDiscoverBodyTrimmed(t *testing.T) {
	root := t.TempDir()
	writeAgent(t, root, "x", "", "\n\n  Body text.  \n\n")
	catalog, _ := Discover([]string{root})
	def, ok := catalog.Lookup("x")
	if !ok || def.Body != "Body text." {
		t.Fatalf("Body = %q, want %q (ok=%v)", def.Body, "Body text.", ok)
	}
}

func TestDiscoverLaterRootWins(t *testing.T) {
	rootA := t.TempDir()
	rootB := t.TempDir()
	writeAgent(t, rootA, "shared", "", "from a\n")
	writeAgent(t, rootB, "shared", "", "from b\n")

	catalog, warnings := Discover([]string{rootA, rootB})
	if len(warnings) != 0 {
		t.Fatalf("warnings = %v, want none", warnings)
	}
	def, ok := catalog.Lookup("shared")
	if !ok {
		t.Fatalf("shared definition not found")
	}
	if def.Body != "from b" {
		t.Fatalf("Body = %q, want from b (root b should win)", def.Body)
	}
}

func TestDiscoverMissingRootSilent(t *testing.T) {
	catalog, warnings := Discover([]string{filepath.Join(t.TempDir(), "does-not-exist")})
	if len(warnings) != 0 {
		t.Fatalf("warnings = %v, want none", warnings)
	}
	if catalog.Len() != 0 {
		t.Fatalf("Len() = %d, want 0", catalog.Len())
	}
}

func TestDiscoverRejectsExternalAgentFileSymlink(t *testing.T) {
	root := t.TempDir()
	dir := filepath.Join(root, "external")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	outside := filepath.Join(t.TempDir(), "AGENT.md")
	if err := os.WriteFile(outside, []byte("---\nname: external\ndescription: secret description\n---\nsecret body\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(outside, filepath.Join(dir, "AGENT.md")); err != nil {
		t.Fatal(err)
	}

	catalog, warnings := Discover([]string{root})
	if catalog.Len() != 0 || len(warnings) != 0 {
		t.Fatalf("catalog=%#v warnings=%v, want external AGENT.md skipped", catalog, warnings)
	}
}

func TestDiscoverSkipsFIFOAgentFileWithoutBlocking(t *testing.T) {
	root := t.TempDir()
	dir := filepath.Join(root, "fifo")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	fifo := filepath.Join(dir, "AGENT.md")
	if err := syscall.Mkfifo(fifo, 0o644); err != nil {
		t.Fatal(err)
	}

	type result struct {
		catalog  Catalog
		warnings []string
	}
	done := make(chan result, 1)
	go func() {
		catalog, warnings := Discover([]string{root})
		done <- result{catalog: catalog, warnings: warnings}
	}()

	var got result
	timedOut := false
	select {
	case got = <-done:
	case <-time.After(2 * time.Second):
		timedOut = true
		writer, err := os.OpenFile(fifo, os.O_WRONLY|syscall.O_NONBLOCK, 0)
		if err == nil {
			_ = writer.Close()
		}
		select {
		case got = <-done:
		case <-time.After(time.Second):
			t.Fatal("Discover blocked opening FIFO AGENT.md")
		}
	}
	if timedOut {
		t.Fatal("Discover blocked opening FIFO AGENT.md")
	}
	if got.catalog.Len() != 0 || len(got.warnings) != 0 {
		t.Fatalf("catalog=%#v warnings=%v, want FIFO skipped", got.catalog, got.warnings)
	}
}

func TestDiscoverRejectsOversizedAgentFile(t *testing.T) {
	root := t.TempDir()
	dir := filepath.Join(root, "large")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(dir, "AGENT.md")
	if err := os.WriteFile(path, []byte("---\nname: large\ndescription: d\n---\nbody\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.Truncate(path, maxAgentFileBytes+1); err != nil {
		t.Fatal(err)
	}

	catalog, warnings := Discover([]string{root})
	if catalog.Len() != 0 || len(warnings) != 1 || !strings.Contains(warnings[0], "too large") {
		t.Fatalf("catalog=%#v warnings=%v, want oversized file warning", catalog, warnings)
	}
}

func TestDiscoverInternalAgentFileSymlink(t *testing.T) {
	root := t.TempDir()
	dir := filepath.Join(root, "linked")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	target := filepath.Join(dir, "definition.md")
	if err := os.WriteFile(target, []byte("---\nname: linked\ndescription: internal\n---\ninternal body\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(filepath.Base(target), filepath.Join(dir, "AGENT.md")); err != nil {
		t.Fatal(err)
	}

	catalog, warnings := Discover([]string{root})
	if len(warnings) != 0 {
		t.Fatalf("warnings=%v, want none", warnings)
	}
	def, ok := catalog.Lookup("linked")
	if !ok || def.Description != "internal" || def.Body != "internal body" {
		t.Fatalf("definition=%#v ok=%v, want internal symlink contents", def, ok)
	}
}

func TestDiscoverSymlinkedDefinitionDirFollowed(t *testing.T) {
	root := t.TempDir()
	realRoot := t.TempDir()
	realDir := writeAgent(t, realRoot, "linked", "", "linked body\n")
	if err := os.Symlink(realDir, filepath.Join(root, "linked")); err != nil {
		t.Fatal(err)
	}

	catalog, warnings := Discover([]string{root})
	if len(warnings) != 0 {
		t.Fatalf("warnings=%v, want none", warnings)
	}
	def, ok := catalog.Lookup("linked")
	if !ok || def.Body != "linked body" || def.Dir != filepath.Join(root, "linked") {
		t.Fatalf("definition=%#v ok=%v, want symlinked definition dir", def, ok)
	}
}

func TestDiscoverDirWithoutAgentMDIgnored(t *testing.T) {
	root := t.TempDir()
	if err := os.MkdirAll(filepath.Join(root, "notanagent"), 0o755); err != nil {
		t.Fatal(err)
	}
	catalog, warnings := Discover([]string{root})
	if len(warnings) != 0 {
		t.Fatalf("warnings = %v, want none", warnings)
	}
	if catalog.Len() != 0 {
		t.Fatalf("Len() = %d, want 0", catalog.Len())
	}
}

func TestDiscoverInvalidFrontmatterWarnsWithPath(t *testing.T) {
	root := t.TempDir()
	dir := filepath.Join(root, "bad")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(dir, "AGENT.md")
	if err := os.WriteFile(path, []byte("no frontmatter here"), 0o644); err != nil {
		t.Fatal(err)
	}
	catalog, warnings := Discover([]string{root})
	if catalog.Len() != 0 {
		t.Fatalf("Len() = %d, want 0", catalog.Len())
	}
	if len(warnings) != 1 || !strings.Contains(warnings[0], path) {
		t.Fatalf("warnings = %v, want one mentioning %s", warnings, path)
	}
}

func TestDefinitionsSorted(t *testing.T) {
	root := t.TempDir()
	writeAgent(t, root, "zebra", "", "body\n")
	writeAgent(t, root, "alpha", "", "body\n")
	writeAgent(t, root, "mid", "", "body\n")

	catalog, warnings := Discover([]string{root})
	if len(warnings) != 0 {
		t.Fatalf("warnings = %v, want none", warnings)
	}
	defs := catalog.Definitions()
	want := []string{"alpha", "mid", "zebra"}
	if len(defs) != len(want) {
		t.Fatalf("Definitions() = %#v, want names %v", defs, want)
	}
	for i, name := range want {
		if defs[i].Name != name {
			t.Fatalf("Definitions()[%d].Name = %q, want %q", i, defs[i].Name, name)
		}
	}
}

func TestCatalogLookupHitAndMiss(t *testing.T) {
	root := t.TempDir()
	writeAgent(t, root, "x", "", "body\n")
	catalog, _ := Discover([]string{root})
	if _, ok := catalog.Lookup("x"); !ok {
		t.Fatalf("Lookup(x) miss, want hit")
	}
	if _, ok := catalog.Lookup("missing"); ok {
		t.Fatalf("Lookup(missing) hit, want miss")
	}
}
