package subagent

import (
	"fmt"
	"html"
	"regexp"
	"strings"
)

// MaxListingBytes caps the rendered prompt section, same as
// skill.MaxListingBytes.
const MaxListingBytes = 8 << 10

const agentsHeader = "\n\n## Agents\n" +
	"Named sub-agent definitions for the `agent` tool (`agent` parameter):\n" +
	"<available_agents>\n"

const agentsFooter = "</available_agents>"

var promptWhitespaceRunPattern = regexp.MustCompile(`\s+`)

// PromptSection renders the "## Agents" system-prompt section, or "" when c
// is empty. Entries that would push the section past MaxListingBytes are
// dropped, and one warning per dropped definition is returned.
func PromptSection(c Catalog) (string, []string) {
	defs := c.Definitions()
	if len(defs) == 0 {
		return "", nil
	}

	var body strings.Builder
	var dropped []string
	for i, d := range defs {
		entry := "<agent name=\"" + html.EscapeString(d.Name) + "\">" +
			html.EscapeString(collapsePromptWhitespace(d.Description)) + "</agent>\n"
		if len(agentsHeader)+body.Len()+len(entry)+len(agentsFooter) > MaxListingBytes {
			for _, remaining := range defs[i:] {
				dropped = append(dropped, remaining.Name)
			}
			break
		}
		body.WriteString(entry)
	}

	var warnings []string
	for _, name := range dropped {
		warnings = append(warnings, fmt.Sprintf("agent %s omitted from prompt: listing exceeds %d bytes", name, MaxListingBytes))
	}
	return agentsHeader + body.String() + agentsFooter, warnings
}

// collapsePromptWhitespace replaces every run of whitespace, including
// newlines, with a single space.
func collapsePromptWhitespace(s string) string {
	return promptWhitespaceRunPattern.ReplaceAllString(s, " ")
}
