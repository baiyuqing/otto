package telegram

import (
	"cmp"
	"slices"
	"strconv"
	"strings"
	"unicode/utf8"

	"github.com/yuin/goldmark"
	"github.com/yuin/goldmark/ast"
	"github.com/yuin/goldmark/extension"
	extast "github.com/yuin/goldmark/extension/ast"
	"github.com/yuin/goldmark/text"
	"github.com/yuin/goldmark/util"
)

// entity is a Bot API MessageEntity. Offset and Length are UTF-16 code units.
type entity struct {
	Type     string `json:"type"`
	Offset   int    `json:"offset"`
	Length   int    `json:"length"`
	URL      string `json:"url,omitempty"`
	Language string `json:"language,omitempty"`
}

// mdParser is CommonMark plus GFM tables and strikethrough. Bare URLs are not
// linkified because Telegram detects them. goldmark parsers are safe for
// concurrent use.
var mdParser = goldmark.New(goldmark.WithExtensions(extension.Table, extension.Strikethrough)).Parser()

// renderMarkdown converts Markdown to plain text and entities. Telegram
// forbids entities inside code and pre; no entity is created inside them.
// The text has no trailing whitespace and entities have non-zero length.
func renderMarkdown(src string) (string, []entity) {
	b := []byte(src)
	r := &renderer{src: b}
	r.blocks(mdParser.Parse(text.NewReader(b)))
	out := strings.TrimRight(r.sb.String(), " \t\r\n")
	total := utf16Len(out)
	var ents []entity
	for _, e := range r.ents {
		e.Length = min(e.Length, total-e.Offset)
		if e.Length > 0 {
			ents = append(ents, e)
		}
	}
	slices.SortStableFunc(ents, func(a, b entity) int {
		return cmp.Or(cmp.Compare(a.Offset, b.Offset), cmp.Compare(b.Length, a.Length))
	})
	return out, ents
}

type renderer struct {
	src     []byte
	sb      strings.Builder
	n       int // UTF-16 units written
	ents    []entity
	indent  string
	inQuote bool
}

func (r *renderer) write(s string) {
	r.sb.WriteString(s)
	r.n += utf16Len(s)
}

func (r *renderer) newline() { r.write("\n" + r.indent) }

func (r *renderer) writeLines(s string) {
	for i, l := range strings.Split(s, "\n") {
		if i > 0 {
			r.newline()
		}
		r.write(l)
	}
}

// span runs body and records an entity over the text it wrote.
func (r *renderer) span(e entity, body func()) {
	e.Offset = r.n
	body()
	e.Length = r.n - e.Offset
	r.ents = append(r.ents, e)
}

func (r *renderer) blocks(parent ast.Node) {
	for c := parent.FirstChild(); c != nil; c = c.NextSibling() {
		if c != parent.FirstChild() {
			r.newline()
			if _, item := parent.(*ast.ListItem); !item {
				r.newline()
			}
		}
		r.block(c)
	}
}

func (r *renderer) lines(n ast.Node) string {
	var sb strings.Builder
	ls := n.Lines()
	for i := range ls.Len() {
		seg := ls.At(i)
		sb.Write(seg.Value(r.src))
	}
	return sb.String()
}

func (r *renderer) block(n ast.Node) {
	switch n := n.(type) {
	case *ast.Paragraph, *ast.TextBlock:
		r.inlines(n)
	case *ast.Heading:
		r.span(entity{Type: "bold"}, func() { r.inlines(n) })
	case *ast.FencedCodeBlock:
		e := entity{Type: "pre", Language: string(n.Language(r.src))}
		r.span(e, func() { r.writeLines(strings.TrimRight(r.lines(n), "\n")) })
	case *ast.CodeBlock:
		r.span(entity{Type: "pre"}, func() { r.writeLines(strings.TrimRight(r.lines(n), "\n")) })
	case *ast.Blockquote:
		if r.inQuote { // Telegram does not allow nested blockquotes.
			r.blocks(n)
			return
		}
		r.inQuote = true
		r.span(entity{Type: "blockquote"}, func() { r.blocks(n) })
		r.inQuote = false
	case *ast.List:
		num := n.Start
		for item := n.FirstChild(); item != nil; item = item.NextSibling() {
			if item != n.FirstChild() {
				r.newline()
			}
			if n.IsOrdered() {
				r.write(strconv.Itoa(num) + ". ")
				num++
			} else {
				r.write("• ")
			}
			saved := r.indent
			r.indent += "  "
			r.blocks(item)
			r.indent = saved
		}
	case *ast.ThematicBreak:
		r.write("──────")
	case *ast.HTMLBlock:
		s := r.lines(n)
		if n.HasClosure() {
			s += string(n.ClosureLine.Value(r.src))
		}
		r.writeLines(strings.TrimRight(s, "\n"))
	case *extast.Table:
		r.span(entity{Type: "pre"}, func() { r.writeLines(r.table(n)) })
	default:
		if n.HasChildren() {
			r.blocks(n)
		} else {
			r.writeLines(strings.TrimRight(r.lines(n), "\n"))
		}
	}
}

// table renders rows as aligned monospace text.
// ponytail: padding counts runes, so East Asian wide characters misalign; a
// display-width count (for example go-runewidth) is the upgrade.
func (r *renderer) table(t *extast.Table) string {
	var rows [][]string
	for row := t.FirstChild(); row != nil; row = row.NextSibling() {
		var cells []string
		for c := row.FirstChild(); c != nil; c = c.NextSibling() {
			cells = append(cells, strings.ReplaceAll(r.plain(c), "\n", " "))
		}
		rows = append(rows, cells)
	}
	var widths []int
	for _, cells := range rows {
		for i, c := range cells {
			if i == len(widths) {
				widths = append(widths, 0)
			}
			widths[i] = max(widths[i], utf8.RuneCountInString(c))
		}
	}
	var out []string
	for ri, cells := range rows {
		parts := make([]string, len(cells))
		for i, c := range cells {
			parts[i] = c + strings.Repeat(" ", widths[i]-utf8.RuneCountInString(c))
		}
		out = append(out, strings.TrimRight(strings.Join(parts, " | "), " "))
		if ri == 0 {
			total := 3 * (len(widths) - 1)
			for _, w := range widths {
				total += w
			}
			out = append(out, strings.Repeat("-", total))
		}
	}
	return strings.Join(out, "\n")
}

func (r *renderer) inlines(n ast.Node) {
	for c := n.FirstChild(); c != nil; c = c.NextSibling() {
		r.inline(c)
	}
}

func (r *renderer) inline(n ast.Node) {
	switch n := n.(type) {
	case *ast.Text:
		r.write(r.text(n))
		if n.HardLineBreak() || n.SoftLineBreak() {
			r.newline()
		}
	case *ast.String:
		r.write(string(n.Value))
	case *ast.Emphasis:
		typ := "italic"
		if n.Level >= 2 {
			typ = "bold"
		}
		r.span(entity{Type: typ}, func() { r.inlines(n) })
	case *extast.Strikethrough:
		r.span(entity{Type: "strikethrough"}, func() { r.inlines(n) })
	case *ast.CodeSpan:
		r.span(entity{Type: "code"}, func() { r.write(strings.ReplaceAll(r.plain(n), "\n", " ")) })
	case *ast.Link:
		if !linkable(n.Destination) {
			r.inlines(n)
			return
		}
		r.span(entity{Type: "text_link", URL: string(n.Destination)}, func() { r.inlines(n) })
	case *ast.Image:
		alt := r.plain(n)
		if alt == "" {
			alt = string(n.Destination)
		}
		if !linkable(n.Destination) {
			r.write(alt)
			return
		}
		r.span(entity{Type: "text_link", URL: string(n.Destination)}, func() { r.write(alt) })
	case *ast.AutoLink:
		r.write(string(n.Label(r.src)))
	case *ast.RawHTML:
		r.write(r.rawHTML(n))
	default:
		r.inlines(n)
	}
}

// linkable reports whether url is absolute with a scheme Telegram opens from a
// text_link entity. Other destinations, such as the relative paths models
// write for repository files, keep only the link text.
func linkable(url []byte) bool {
	s := strings.ToLower(string(url))
	return strings.HasPrefix(s, "http://") || strings.HasPrefix(s, "https://") || strings.HasPrefix(s, "tg://")
}

// text is the text of n with backslash escapes and entity references
// resolved, as goldmark's HTML renderer does. Code span text is raw and is
// kept as written.
// ponytail: the three passes resolve "\&amp;" to "&", where CommonMark keeps
// "&amp;"; a single-pass unescape is the upgrade if that ever matters.
func (r *renderer) text(n *ast.Text) string {
	v := n.Segment.Value(r.src)
	if !n.IsRaw() {
		v = util.ResolveEntityNames(util.ResolveNumericReferences(util.UnescapePunctuations(v)))
	}
	return string(v)
}

// rawHTML is the literal source of a RawHTML node.
func (r *renderer) rawHTML(n *ast.RawHTML) string {
	var sb strings.Builder
	for i := range n.Segments.Len() {
		seg := n.Segments.At(i)
		sb.Write(seg.Value(r.src))
	}
	return sb.String()
}

// plain is the text of n without formatting; breaks become spaces.
func (r *renderer) plain(n ast.Node) string {
	switch n := n.(type) {
	case *ast.Text:
		s := r.text(n)
		if n.HardLineBreak() || n.SoftLineBreak() {
			s += " "
		}
		return s
	case *ast.String:
		return string(n.Value)
	case *ast.AutoLink:
		return string(n.Label(r.src))
	case *ast.RawHTML:
		return r.rawHTML(n)
	}
	var sb strings.Builder
	for c := n.FirstChild(); c != nil; c = c.NextSibling() {
		sb.WriteString(r.plain(c))
	}
	return sb.String()
}
