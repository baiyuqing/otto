package telegram

import (
	"reflect"
	"testing"
)

func TestRenderMarkdown(t *testing.T) {
	cases := []struct {
		name, in, text string
		ents           []entity
	}{
		{"bold italic nested", "a **b *c* d** e", "a b c d e", []entity{
			{Type: "bold", Offset: 2, Length: 5}, {Type: "italic", Offset: 4, Length: 1}}},
		{"strikethrough", "~~gone~~", "gone", []entity{{Type: "strikethrough", Offset: 0, Length: 4}}},
		{"code span", "run `ls -l` now", "run ls -l now", []entity{{Type: "code", Offset: 4, Length: 5}}},
		{"fenced code with language", "```go extra\nx := 1\ny := 2\n```", "x := 1\ny := 2",
			[]entity{{Type: "pre", Offset: 0, Length: 13, Language: "go"}}},
		{"fenced code without language and no nesting", "```\n**not bold**\n```", "**not bold**",
			[]entity{{Type: "pre", Offset: 0, Length: 12}}},
		{"link", "see [docs](https://example.com/a) ok", "see docs ok",
			[]entity{{Type: "text_link", Offset: 4, Length: 4, URL: "https://example.com/a"}}},
		{"image", "![logo](https://example.com/i.png)", "logo",
			[]entity{{Type: "text_link", Offset: 0, Length: 4, URL: "https://example.com/i.png"}}},
		{"relative link keeps text only", "see [mod.rs](crates/otto/src/acp/mod.rs#L278) ok", "see mod.rs ok", nil},
		{"relative image keeps alt text only", "![diagram](docs/a.png)", "diagram", nil},
		{"autolink", "<https://example.com>", "https://example.com", nil},
		{"heading and paragraph", "# Title\n\nbody", "Title\n\nbody", []entity{{Type: "bold", Offset: 0, Length: 5}}},
		{"soft and hard breaks", "a\nb  \nc", "a\nb\nc", nil},
		{"lists", "- one\n  - nested\n- two\n\n3. x\n4. y", "• one\n  • nested\n• two\n\n3. x\n4. y", nil},
		{"blockquote", "> q1\n> q2\n\nafter", "q1\nq2\n\nafter", []entity{{Type: "blockquote", Offset: 0, Length: 5}}},
		{"table", "| a | bcd |\n|---|---|\n| ee | f |", "a  | bcd\n--------\nee | f",
			[]entity{{Type: "pre", Offset: 0, Length: 24}}},
		{"thematic break", "a\n\n---\n\nb", "a\n\n──────\n\nb", nil},
		{"raw html stays literal", "use <id> and <b>x</b>\n\n<div>y</div>", "use <id> and <b>x</b>\n\n<div>y</div>", nil},
		{"utf16 offsets", "😀 中文 **粗**", "😀 中文 粗", []entity{{Type: "bold", Offset: 6, Length: 1}}},
		{"escapes and entity references resolved", `a \*b\* snake\_case &amp; &lt;x&gt; &#169;`,
			"a *b* snake_case & <x> ©", nil},
		{"code span keeps escapes", "`a \\* &amp;`", `a \* &amp;`, []entity{{Type: "code", Offset: 0, Length: 10}}},
		{"escaped pipe in table cell", "| \\|a | b |\n|---|---|\n| 1 | 2 |", "|a | b\n------\n1  | 2",
			[]entity{{Type: "pre", Offset: 0, Length: 20}}},
		{"trailing whitespace trimmed", "hi\n\n", "hi", nil},
	}
	for _, c := range cases {
		text, ents := renderMarkdown(c.in)
		if text != c.text || !reflect.DeepEqual(ents, c.ents) {
			t.Errorf("%s: renderMarkdown(%q)\n got %q %+v\nwant %q %+v", c.name, c.in, text, ents, c.text, c.ents)
		}
	}
}
