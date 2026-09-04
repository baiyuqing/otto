package skill

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func writeSkill(t *testing.T, root, name, frontmatterExtra, body string) string {
	t.Helper()
	dir := filepath.Join(root, name)
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	content := "---\nname: " + name + "\ndescription: desc for " + name + "\n" + frontmatterExtra + "---\n" + body
	if err := os.WriteFile(filepath.Join(dir, "SKILL.md"), []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
	return dir
}

func TestDiscoverFindsSkillsAcrossTwoRootsWithOverride(t *testing.T) {
	userRoot := t.TempDir()
	workspaceRoot := t.TempDir()
	writeSkill(t, userRoot, "pdf", "", "# pdf\n")
	writeSkill(t, userRoot, "shared", "", "# user shared\n")
	writeSkill(t, workspaceRoot, "shared", "", "# workspace shared\n")

	catalog, warnings := Discover([]string{userRoot, workspaceRoot})
	if len(warnings) != 0 {
		t.Fatalf("warnings = %v, want none", warnings)
	}
	if catalog.Len() != 2 {
		t.Fatalf("Len() = %d, want 2", catalog.Len())
	}
	shared, ok := catalog.Lookup("shared")
	if !ok {
		t.Fatalf("shared skill not found")
	}
	if shared.Dir != filepath.Join(workspaceRoot, "shared") {
		t.Fatalf("shared.Dir = %q, want workspace root to win", shared.Dir)
	}
	skills := catalog.Skills()
	if len(skills) != 2 || skills[0].Name != "pdf" || skills[1].Name != "shared" {
		t.Fatalf("Skills() = %#v, want sorted [pdf shared]", skills)
	}
}

func TestDiscoverMissingRootIsSilent(t *testing.T) {
	catalog, warnings := Discover([]string{filepath.Join(t.TempDir(), "does-not-exist")})
	if len(warnings) != 0 {
		t.Fatalf("warnings = %v, want none", warnings)
	}
	if catalog.Len() != 0 {
		t.Fatalf("Len() = %d, want 0", catalog.Len())
	}
}

func TestDiscoverUnreadableRootWarns(t *testing.T) {
	if os.Geteuid() == 0 {
		t.Skip("running as root; chmod 000 does not block reads")
	}
	root := t.TempDir()
	blocked := filepath.Join(root, "blocked")
	if err := os.Mkdir(blocked, 0o000); err != nil {
		t.Fatal(err)
	}
	defer os.Chmod(blocked, 0o755)

	_, warnings := Discover([]string{blocked})
	if len(warnings) != 1 || !strings.Contains(warnings[0], "skills root") || !strings.Contains(warnings[0], blocked) {
		t.Fatalf("warnings = %v, want one mentioning skills root and path", warnings)
	}
}

func TestDiscoverDirectoryWithoutSkillMDIgnored(t *testing.T) {
	root := t.TempDir()
	if err := os.MkdirAll(filepath.Join(root, "notaskill"), 0o755); err != nil {
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

func TestDiscoverInvalidSkillWarnsAndOthersStillLoad(t *testing.T) {
	root := t.TempDir()
	writeSkill(t, root, "good", "", "# good\n")
	badDir := filepath.Join(root, "bad")
	if err := os.MkdirAll(badDir, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(badDir, "SKILL.md"), []byte("no frontmatter here"), 0o644); err != nil {
		t.Fatal(err)
	}

	catalog, warnings := Discover([]string{root})
	if catalog.Len() != 1 {
		t.Fatalf("Len() = %d, want 1", catalog.Len())
	}
	if _, ok := catalog.Lookup("good"); !ok {
		t.Fatalf("good skill missing")
	}
	if len(warnings) != 1 || !strings.Contains(warnings[0], filepath.Join(badDir, "SKILL.md")) {
		t.Fatalf("warnings = %v, want one mentioning %s", warnings, badDir)
	}
}

func TestDiscoverSymlinkedSkillDirFollowed(t *testing.T) {
	root := t.TempDir()
	real := t.TempDir()
	writeSkill(t, real, "linked", "", "# linked\n")
	if err := os.Symlink(filepath.Join(real, "linked"), filepath.Join(root, "linked")); err != nil {
		t.Fatal(err)
	}

	catalog, warnings := Discover([]string{root})
	if len(warnings) != 0 {
		t.Fatalf("warnings = %v, want none", warnings)
	}
	skill, ok := catalog.Lookup("linked")
	if !ok {
		t.Fatalf("linked skill not found")
	}
	if skill.Dir != filepath.Join(root, "linked") {
		t.Fatalf("Dir = %q, want root/linked (no EvalSymlinks)", skill.Dir)
	}
}

func TestDiscoverRejectsSkillFileSymlinkEscape(t *testing.T) {
	root := t.TempDir()
	dir := filepath.Join(root, "escaped")
	if err := os.Mkdir(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	outside := filepath.Join(t.TempDir(), "SKILL.md")
	if err := os.WriteFile(outside, []byte("---\nname: escaped\ndescription: outside\n---\nbody\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(outside, filepath.Join(dir, "SKILL.md")); err != nil {
		t.Fatal(err)
	}

	catalog, warnings := Discover([]string{root})
	if catalog.Len() != 0 || len(warnings) != 1 {
		t.Fatalf("catalog=%#v warnings=%v, want one rejected skill", catalog, warnings)
	}
}

func TestDiscoverNameValidation(t *testing.T) {
	tests := []struct {
		name   string
		dir    string
		nameFM string
		reason string
	}{
		{"uppercase", "Upper", "Upper", "invalid"},
		{"leading hyphen", "-lead", "-lead", "invalid"},
		{"trailing hyphen", "trail-", "trail-", "invalid"},
		{"double hyphen", "dou--ble", "dou--ble", "invalid"},
		{"too long", strings.Repeat("a", 65), strings.Repeat("a", 65), "invalid"},
		{"directory mismatch", "actualdir", "otherdir", "does not match directory"},
		{"missing name", "noname", "", "missing name"},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			root := t.TempDir()
			dir := filepath.Join(root, tt.dir)
			if err := os.MkdirAll(dir, 0o755); err != nil {
				t.Fatal(err)
			}
			nameLine := ""
			if tt.nameFM != "" {
				nameLine = "name: " + tt.nameFM + "\n"
			}
			content := "---\n" + nameLine + "description: d\n---\nbody\n"
			if err := os.WriteFile(filepath.Join(dir, "SKILL.md"), []byte(content), 0o644); err != nil {
				t.Fatal(err)
			}
			catalog, warnings := Discover([]string{root})
			if catalog.Len() != 0 {
				t.Fatalf("Len() = %d, want 0 for invalid skill", catalog.Len())
			}
			if len(warnings) != 1 || !strings.Contains(warnings[0], tt.reason) {
				t.Fatalf("warnings = %v, want one containing %q", warnings, tt.reason)
			}
		})
	}
}

func TestDiscoverDescriptionValidation(t *testing.T) {
	t.Run("missing", func(t *testing.T) {
		root := t.TempDir()
		dir := filepath.Join(root, "x")
		if err := os.MkdirAll(dir, 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(dir, "SKILL.md"), []byte("---\nname: x\n---\nbody\n"), 0o644); err != nil {
			t.Fatal(err)
		}
		catalog, warnings := Discover([]string{root})
		if catalog.Len() != 0 || len(warnings) != 1 || !strings.Contains(warnings[0], "missing description") {
			t.Fatalf("catalog.Len()=%d warnings=%v, want 0 and missing description", catalog.Len(), warnings)
		}
	})

	t.Run("too long", func(t *testing.T) {
		root := t.TempDir()
		dir := filepath.Join(root, "x")
		if err := os.MkdirAll(dir, 0o755); err != nil {
			t.Fatal(err)
		}
		long := strings.Repeat("a", 1025)
		content := "---\nname: x\ndescription: " + long + "\n---\nbody\n"
		if err := os.WriteFile(filepath.Join(dir, "SKILL.md"), []byte(content), 0o644); err != nil {
			t.Fatal(err)
		}
		catalog, warnings := Discover([]string{root})
		if catalog.Len() != 0 || len(warnings) != 1 || !strings.Contains(warnings[0], "exceeds 1024") {
			t.Fatalf("catalog.Len()=%d warnings=%v, want 0 and exceeds 1024", catalog.Len(), warnings)
		}
	})

	t.Run("trimmed", func(t *testing.T) {
		root := t.TempDir()
		dir := filepath.Join(root, "x")
		if err := os.MkdirAll(dir, 0o755); err != nil {
			t.Fatal(err)
		}
		content := "---\nname: x\ndescription: \"  padded  \"\n---\nbody\n"
		if err := os.WriteFile(filepath.Join(dir, "SKILL.md"), []byte(content), 0o644); err != nil {
			t.Fatal(err)
		}
		catalog, warnings := Discover([]string{root})
		if len(warnings) != 0 {
			t.Fatalf("warnings = %v, want none", warnings)
		}
		s, ok := catalog.Lookup("x")
		if !ok || s.Description != "padded" {
			t.Fatalf("Description = %q, want %q", s.Description, "padded")
		}
	})
}

func TestLoadStripsFrontmatter(t *testing.T) {
	root := t.TempDir()
	dir := writeSkill(t, root, "pdf", "", "# PDF handling\nBody text\n")
	catalog, warnings := Discover([]string{root})
	if len(warnings) != 0 {
		t.Fatalf("warnings = %v", warnings)
	}
	s, ok := catalog.Lookup("pdf")
	if !ok {
		t.Fatalf("pdf skill not found")
	}
	if s.Dir != dir || s.Path != filepath.Join(dir, "SKILL.md") {
		t.Fatalf("Dir/Path = %q/%q", s.Dir, s.Path)
	}
	body, err := Load(s)
	if err != nil {
		t.Fatalf("Load: %v", err)
	}
	if body != "# PDF handling\nBody text\n" {
		t.Fatalf("body = %q", body)
	}
}

func TestLoadHonorsPathWithinSkillDirectory(t *testing.T) {
	dir := writeSkill(t, t.TempDir(), "sample", "", "default\n")
	path := filepath.Join(dir, "alternate.md")
	if err := os.WriteFile(path, []byte("---\nname: sample\n---\nalternate\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	body, err := Load(Skill{Dir: dir, Path: path})
	if err != nil || strings.TrimSpace(body) != "alternate" {
		t.Fatalf("Load() = %q, %v, want the selected file", body, err)
	}
	outside := writeSkill(t, t.TempDir(), "outside", "", "external\n")
	if _, err := Load(Skill{Dir: dir, Path: filepath.Join(outside, "SKILL.md")}); err == nil {
		t.Fatal("Load accepted a path outside the skill directory")
	}
}

func TestLoadRejectsOversizedSkillFile(t *testing.T) {
	root := t.TempDir()
	dir := writeSkill(t, root, "pdf", "", "body\n")
	if err := os.Truncate(filepath.Join(dir, "SKILL.md"), maxSkillFileBytes+1); err != nil {
		t.Fatal(err)
	}
	if _, err := Load(Skill{Dir: dir, Path: filepath.Join(dir, "SKILL.md")}); err == nil || !strings.Contains(err.Error(), "too large") {
		t.Fatalf("Load() error = %v, want size rejection", err)
	}
}

func TestListFilesOrderingHiddenAndSymlinkSkip(t *testing.T) {
	root := t.TempDir()
	must := func(err error) {
		t.Helper()
		if err != nil {
			t.Fatal(err)
		}
	}
	must(os.WriteFile(filepath.Join(root, "SKILL.md"), []byte("x"), 0o644))
	must(os.WriteFile(filepath.Join(root, "b.txt"), []byte("x"), 0o644))
	must(os.MkdirAll(filepath.Join(root, "scripts"), 0o755))
	must(os.WriteFile(filepath.Join(root, "scripts", "a.py"), []byte("x"), 0o644))
	must(os.WriteFile(filepath.Join(root, ".hidden"), []byte("x"), 0o644))
	must(os.MkdirAll(filepath.Join(root, ".hiddendir"), 0o755))
	must(os.WriteFile(filepath.Join(root, ".hiddendir", "inner.txt"), []byte("x"), 0o644))

	outsideFile := filepath.Join(t.TempDir(), "outside.txt")
	must(os.WriteFile(outsideFile, []byte("x"), 0o644))
	must(os.Symlink(outsideFile, filepath.Join(root, "link.txt")))

	files, total, err := ListFiles(root, 50)
	if err != nil {
		t.Fatalf("ListFiles: %v", err)
	}
	want := []string{"b.txt", "scripts/a.py"}
	if len(files) != len(want) {
		t.Fatalf("files = %v, want %v", files, want)
	}
	for i := range want {
		if files[i] != want[i] {
			t.Fatalf("files = %v, want %v", files, want)
		}
	}
	if total != len(want) {
		t.Fatalf("total = %d, want %d", total, len(want))
	}
}

func TestListFilesLimitAndTotal(t *testing.T) {
	root := t.TempDir()
	for i := 0; i < 5; i++ {
		name := filepath.Join(root, "f"+string(rune('a'+i))+".txt")
		if err := os.WriteFile(name, []byte("x"), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	files, total, err := ListFiles(root, 3)
	if err != nil {
		t.Fatalf("ListFiles: %v", err)
	}
	if len(files) != 3 {
		t.Fatalf("len(files) = %d, want 3", len(files))
	}
	if total != 5 {
		t.Fatalf("total = %d, want 5", total)
	}
}

func TestListFilesEmpty(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "SKILL.md"), []byte("x"), 0o644); err != nil {
		t.Fatal(err)
	}
	files, total, err := ListFiles(root, 50)
	if err != nil {
		t.Fatalf("ListFiles: %v", err)
	}
	if len(files) != 0 || total != 0 {
		t.Fatalf("files=%v total=%d, want empty", files, total)
	}
}

func TestPromptSectionEmptyCatalog(t *testing.T) {
	section, warnings := PromptSection(Catalog{})
	if section != "" {
		t.Fatalf("section = %q, want empty", section)
	}
	if len(warnings) != 0 {
		t.Fatalf("warnings = %v, want none", warnings)
	}
}

func TestPromptSectionRendersAndEscapes(t *testing.T) {
	root := t.TempDir()
	dir := filepath.Join(root, "pdf")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	content := "---\nname: pdf\ndescription: |\n  Handles <PDF> & \"forms\"\n  across lines\n---\nbody\n"
	if err := os.WriteFile(filepath.Join(dir, "SKILL.md"), []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
	catalog, warnings := Discover([]string{root})
	if len(warnings) != 0 {
		t.Fatalf("warnings = %v", warnings)
	}

	section, promptWarnings := PromptSection(catalog)
	if len(promptWarnings) != 0 {
		t.Fatalf("promptWarnings = %v, want none", promptWarnings)
	}
	if !strings.HasPrefix(section, "\n\n## Skills\n") {
		t.Fatalf("section does not start with expected header: %q", section)
	}
	if !strings.Contains(section, "<available_skills>\n") || !strings.HasSuffix(section, "</available_skills>\n") {
		t.Fatalf("section missing available_skills wrapper: %q", section)
	}
	if !strings.Contains(section, `<skill name="pdf" location="`+dir+`">`) {
		t.Fatalf("section missing skill entry: %q", section)
	}
	if !strings.Contains(section, "Handles &lt;PDF&gt; &amp; &#34;forms&#34; across lines</skill>") {
		t.Fatalf("section did not escape and collapse whitespace: %q", section)
	}
	if strings.Contains(section, "<PDF>") {
		t.Fatalf("section leaked unescaped markup: %q", section)
	}
}

func TestPromptSectionByteCapDropsWithWarning(t *testing.T) {
	root := t.TempDir()
	longDescription := strings.Repeat("a", 1024)
	names := []string{"one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten"}
	for _, name := range names {
		dir := filepath.Join(root, name)
		if err := os.MkdirAll(dir, 0o755); err != nil {
			t.Fatal(err)
		}
		content := "---\nname: " + name + "\ndescription: " + longDescription + "\n---\nbody\n"
		if err := os.WriteFile(filepath.Join(dir, "SKILL.md"), []byte(content), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	catalog, warnings := Discover([]string{root})
	if len(warnings) != 0 {
		t.Fatalf("warnings = %v", warnings)
	}
	if catalog.Len() != len(names) {
		t.Fatalf("Len() = %d, want %d", catalog.Len(), len(names))
	}

	section, promptWarnings := PromptSection(catalog)
	if len(section) > MaxListingBytes {
		t.Fatalf("section length %d exceeds cap %d", len(section), MaxListingBytes)
	}
	if len(promptWarnings) != 1 || !strings.Contains(promptWarnings[0], "dropped") {
		t.Fatalf("promptWarnings = %v, want one mentioning dropped skills", promptWarnings)
	}
}

func TestCatalogZeroValueIsUsable(t *testing.T) {
	var c Catalog
	if c.Len() != 0 {
		t.Fatalf("Len() = %d, want 0", c.Len())
	}
	if skills := c.Skills(); len(skills) != 0 {
		t.Fatalf("Skills() = %v, want empty", skills)
	}
	if _, ok := c.Lookup("x"); ok {
		t.Fatalf("Lookup found something in zero-value catalog")
	}
}
