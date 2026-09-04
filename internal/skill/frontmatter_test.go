package skill

import (
	"strings"
	"testing"
)

func TestParseFrontmatterPlainScalar(t *testing.T) {
	fields, body, err := ParseFrontmatter([]byte("---\nname: pdf\ndescription: Extract text from PDFs\n---\n# PDF\n"))
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if fields["name"] != "pdf" {
		t.Fatalf("name = %q, want %q", fields["name"], "pdf")
	}
	if fields["description"] != "Extract text from PDFs" {
		t.Fatalf("description = %q", fields["description"])
	}
	if body != "# PDF\n" {
		t.Fatalf("body = %q", body)
	}
}

func TestParseFrontmatterPlainScalarMultiLine(t *testing.T) {
	data := "---\ndescription: Extract text\n  from PDFs\n  and merge them\nname: pdf\n---\nbody\n"
	fields, _, err := ParseFrontmatter([]byte(data))
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	want := "Extract text from PDFs and merge them"
	if fields["description"] != want {
		t.Fatalf("description = %q, want %q", fields["description"], want)
	}
}

func TestParseFrontmatterDoubleQuotedWithEscapes(t *testing.T) {
	data := `---
description: "Say \"hi\"\nnext line\tend\\done"
---
body
`
	fields, _, err := ParseFrontmatter([]byte(data))
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	want := "Say \"hi\"\nnext line\tend\\done"
	if fields["description"] != want {
		t.Fatalf("description = %q, want %q", fields["description"], want)
	}
}

func TestParseFrontmatterDoubleQuotedUnknownEscape(t *testing.T) {
	data := `---
name: "a\qb"
---
body
`
	fields, _, err := ParseFrontmatter([]byte(data))
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if fields["name"] != "aqb" {
		t.Fatalf("name = %q, want %q", fields["name"], "aqb")
	}
}

func TestParseFrontmatterSingleQuotedEscape(t *testing.T) {
	data := "---\ndescription: 'it''s a test'\n---\nbody\n"
	fields, _, err := ParseFrontmatter([]byte(data))
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	want := "it's a test"
	if fields["description"] != want {
		t.Fatalf("description = %q, want %q", fields["description"], want)
	}
}

func TestParseFrontmatterLiteralBlock(t *testing.T) {
	data := "---\ndescription: |\n  line one\n  line two\nname: x\n---\nbody\n"
	fields, _, err := ParseFrontmatter([]byte(data))
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	want := "line one\nline two\n"
	if fields["description"] != want {
		t.Fatalf("description = %q, want %q", fields["description"], want)
	}
}

func TestParseFrontmatterLiteralBlockWithChompingIndicator(t *testing.T) {
	data := "---\ndescription: |-\n  line one\n  line two\n---\nbody\n"
	fields, _, err := ParseFrontmatter([]byte(data))
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	want := "line one\nline two\n"
	if fields["description"] != want {
		t.Fatalf("description = %q, want %q", fields["description"], want)
	}
}

func TestParseFrontmatterFoldedBlockWithBlankLine(t *testing.T) {
	data := "---\ndescription: >\n  This is line one\n  continued.\n\n  Second paragraph.\n---\nbody\n"
	fields, _, err := ParseFrontmatter([]byte(data))
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	want := "This is line one continued.\nSecond paragraph.\n"
	if fields["description"] != want {
		t.Fatalf("description = %q, want %q", fields["description"], want)
	}
}

func TestParseFrontmatterNestedBlockSkipped(t *testing.T) {
	data := "---\nname: pdf\nmetadata:\n  category: files\n  version: \"1.0\"\ndescription: d\n---\nbody\n"
	fields, _, err := ParseFrontmatter([]byte(data))
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if fields["name"] != "pdf" || fields["description"] != "d" {
		t.Fatalf("fields = %#v", fields)
	}
	if _, ok := fields["category"]; ok {
		t.Fatalf("nested key leaked into top-level fields: %#v", fields)
	}
}

func TestParseFrontmatterCommentsIgnored(t *testing.T) {
	// The indented comment sits after a double-quoted value, which never
	// consumes continuation lines, so it is read as a standalone comment
	// rather than folded into the previous scalar.
	data := "---\n# a comment\nname: \"pdf\"\n  # indented comment between blocks\ndescription: d\n---\nbody\n"
	fields, _, err := ParseFrontmatter([]byte(data))
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if fields["name"] != "pdf" || fields["description"] != "d" {
		t.Fatalf("fields = %#v", fields)
	}
}

func TestParseFrontmatterDuplicateKeyLastWins(t *testing.T) {
	data := "---\nname: first\nname: second\n---\nbody\n"
	fields, _, err := ParseFrontmatter([]byte(data))
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if fields["name"] != "second" {
		t.Fatalf("name = %q, want %q", fields["name"], "second")
	}
}

func TestParseFrontmatterMissingFrontmatter(t *testing.T) {
	_, _, err := ParseFrontmatter([]byte("# just markdown\n"))
	if err == nil || !strings.Contains(err.Error(), "missing frontmatter") {
		t.Fatalf("err = %v, want missing frontmatter", err)
	}
}

func TestParseFrontmatterUnterminated(t *testing.T) {
	_, _, err := ParseFrontmatter([]byte("---\nname: pdf\n"))
	if err == nil || !strings.Contains(err.Error(), "unterminated frontmatter") {
		t.Fatalf("err = %v, want unterminated frontmatter", err)
	}
}

func TestParseFrontmatterUnsupportedLine(t *testing.T) {
	_, _, err := ParseFrontmatter([]byte("---\nnot a key line\n---\nbody\n"))
	if err == nil || !strings.Contains(err.Error(), "unsupported frontmatter line 2") {
		t.Fatalf("err = %v, want unsupported frontmatter line 2", err)
	}
}

func TestParseFrontmatterUnterminatedDoubleQuoted(t *testing.T) {
	_, _, err := ParseFrontmatter([]byte("---\nname: \"unterminated\n---\nbody\n"))
	if err == nil {
		t.Fatalf("expected error for unterminated double-quoted value")
	}
}

func TestParseFrontmatterUnterminatedSingleQuoted(t *testing.T) {
	_, _, err := ParseFrontmatter([]byte("---\nname: 'unterminated\n---\nbody\n"))
	if err == nil {
		t.Fatalf("expected error for unterminated single-quoted value")
	}
}

func TestParseFrontmatterDelimiterToleratesCR(t *testing.T) {
	data := "---\r\nname: pdf\r\n---\r\nbody\r\n"
	fields, _, err := ParseFrontmatter([]byte(data))
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if fields["name"] != "pdf" {
		t.Fatalf("name = %q", fields["name"])
	}
}

func TestParseFrontmatterEmptyValue(t *testing.T) {
	data := "---\nname:\ndescription: d\n---\nbody\n"
	fields, _, err := ParseFrontmatter([]byte(data))
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if fields["name"] != "" {
		t.Fatalf("name = %q, want empty", fields["name"])
	}
}

func TestParseFrontmatterBodyLeadingNewlineRemovedOnce(t *testing.T) {
	fields, body, err := ParseFrontmatter([]byte("---\nname: pdf\n---\n\n\n# heading\n"))
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	_ = fields
	if body != "\n# heading\n" {
		t.Fatalf("body = %q, want %q", body, "\n# heading\n")
	}
}
