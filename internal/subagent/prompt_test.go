package subagent

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// writeAgentDescription writes an AGENT.md with an explicit (possibly
// multi-line, via a YAML literal block) description, for prompt-rendering
// tests that need to control the description text exactly.
func writeAgentDescription(t *testing.T, root, name, description string) string {
	t.Helper()
	dir := filepath.Join(root, name)
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	content := "---\nname: " + name + "\ndescription: |\n"
	for _, line := range strings.Split(description, "\n") {
		content += "  " + line + "\n"
	}
	content += "---\nbody\n"
	if err := os.WriteFile(filepath.Join(dir, "AGENT.md"), []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
	return dir
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

func TestPromptSectionTwoDefinitionsExactString(t *testing.T) {
	root := t.TempDir()
	writeAgent(t, root, "alpha", "", "body\n")
	writeAgent(t, root, "beta", "", "body\n")
	catalog, warnings := Discover([]string{root})
	if len(warnings) != 0 {
		t.Fatalf("warnings = %v, want none", warnings)
	}

	section, promptWarnings := PromptSection(catalog)
	if len(promptWarnings) != 0 {
		t.Fatalf("promptWarnings = %v, want none", promptWarnings)
	}
	want := "\n\n## Agents\n" +
		"Named sub-agent definitions for the `agent` tool (`agent` parameter):\n" +
		"<available_agents>\n" +
		`<agent name="alpha">desc for alpha</agent>` + "\n" +
		`<agent name="beta">desc for beta</agent>` + "\n" +
		"</available_agents>"
	if section != want {
		t.Fatalf("section = %q, want %q", section, want)
	}
}

func TestPromptSectionEscapesHTML(t *testing.T) {
	root := t.TempDir()
	writeAgentDescription(t, root, "x", `Review <diffs> & "reports"`)
	catalog, warnings := Discover([]string{root})
	if len(warnings) != 0 {
		t.Fatalf("warnings = %v, want none", warnings)
	}
	section, promptWarnings := PromptSection(catalog)
	if len(promptWarnings) != 0 {
		t.Fatalf("promptWarnings = %v, want none", promptWarnings)
	}
	if !strings.Contains(section, "Review &lt;diffs&gt; &amp; &#34;reports&#34;</agent>") {
		t.Fatalf("section did not escape description: %q", section)
	}
	if strings.Contains(section, "<diffs>") {
		t.Fatalf("section leaked unescaped markup: %q", section)
	}
}

func TestPromptSectionCollapsesWhitespace(t *testing.T) {
	root := t.TempDir()
	writeAgentDescription(t, root, "x", "line one\nline   two")
	catalog, warnings := Discover([]string{root})
	if len(warnings) != 0 {
		t.Fatalf("warnings = %v, want none", warnings)
	}
	section, promptWarnings := PromptSection(catalog)
	if len(promptWarnings) != 0 {
		t.Fatalf("promptWarnings = %v, want none", promptWarnings)
	}
	if !strings.Contains(section, "line one line two</agent>") {
		t.Fatalf("section did not collapse whitespace: %q", section)
	}
}

func TestPromptSectionByteCapDropsLaterDefinition(t *testing.T) {
	root := t.TempDir()
	long := strings.Repeat("a", 1024)
	names := []string{"one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten"}
	for _, name := range names {
		writeAgentDescription(t, root, name, long)
	}
	catalog, warnings := Discover([]string{root})
	if len(warnings) != 0 {
		t.Fatalf("warnings = %v, want none", warnings)
	}
	if catalog.Len() != len(names) {
		t.Fatalf("Len() = %d, want %d", catalog.Len(), len(names))
	}

	section, promptWarnings := PromptSection(catalog)
	if len(section) > MaxListingBytes {
		t.Fatalf("section length %d exceeds cap %d", len(section), MaxListingBytes)
	}
	if len(promptWarnings) == 0 {
		t.Fatalf("promptWarnings empty, want at least one drop warning")
	}
	for _, w := range promptWarnings {
		if !strings.Contains(w, "omitted from prompt") {
			t.Fatalf("warning %q does not mention omitted from prompt", w)
		}
	}
}
