package subagent

import (
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"unicode/utf8"

	"github.com/baiyuqing/otto/internal/skill"
)

// Definition is one discovered named sub-agent definition (AGENT.md).
type Definition struct {
	// Name is the frontmatter name, equal to the directory base name.
	Name string
	// Description is the frontmatter description, trimmed.
	Description string
	// Tools is the frontmatter tools allowlist, or nil when the frontmatter
	// has no tools key (meaning "all child tools").
	Tools []string
	// Model is the frontmatter model id, or "" to use the caller's/session
	// model.
	Model string
	// Context is "fresh" (default) or "inherit".
	Context string
	// Body is the Markdown after the frontmatter, TrimSpace'd; "" allowed.
	Body string
	// Dir is the absolute directory containing AGENT.md.
	Dir string
	// Path is the absolute path of AGENT.md.
	Path string
}

// Catalog is the set of agent definitions discovered for one runner. The
// zero value is empty and usable.
type Catalog struct {
	definitions []Definition // sorted by Name
}

// Definitions returns the catalog's definitions sorted by name. The
// returned slice is a copy.
func (c Catalog) Definitions() []Definition {
	out := make([]Definition, len(c.definitions))
	copy(out, c.definitions)
	return out
}

// Lookup returns the definition with the given name, if any.
func (c Catalog) Lookup(name string) (Definition, bool) {
	for _, d := range c.definitions {
		if d.Name == name {
			return d, true
		}
	}
	return Definition{}, false
}

// Len returns the number of definitions in the catalog.
func (c Catalog) Len() int {
	return len(c.definitions)
}

// agentNamePattern and the length/description limits mirror
// internal/skill's skill-name and description rules exactly (copied rather
// than exported from that package).
var agentNamePattern = regexp.MustCompile(`^[a-z0-9]+(-[a-z0-9]+)*$`)

const maxAgentNameLength = 64
const maxAgentDescriptionRunes = 1024

var agentToolNamePattern = regexp.MustCompile(`^[a-z0-9_]+$`)

// Discover scans roots in order; a later root overrides an earlier one on
// the same name. roots are absolute directories. A missing root is skipped
// silently. An unreadable root or an invalid agent directory produces one
// warning string and is skipped. Discover never errors.
func Discover(roots []string) (Catalog, []string) {
	var warnings []string
	byName := make(map[string]Definition)

	for _, root := range roots {
		entries, err := os.ReadDir(root)
		if err != nil {
			if errors.Is(err, fs.ErrNotExist) {
				continue
			}
			warnings = append(warnings, fmt.Sprintf("agents root %s: %s", root, err))
			continue
		}
		for _, entry := range entries {
			if !entryIsDir(root, entry) {
				continue
			}
			dir := filepath.Join(root, entry.Name())
			agentPath := filepath.Join(dir, "AGENT.md")
			info, err := os.Stat(agentPath)
			if err != nil || !info.Mode().IsRegular() {
				// A directory without AGENT.md is silently ignored.
				continue
			}
			def, warning := loadCandidate(dir, agentPath, entry.Name())
			if warning != "" {
				warnings = append(warnings, warning)
				continue
			}
			byName[def.Name] = def
		}
	}

	names := make([]string, 0, len(byName))
	for name := range byName {
		names = append(names, name)
	}
	sort.Strings(names)
	defs := make([]Definition, len(names))
	for i, name := range names {
		defs[i] = byName[name]
	}
	return Catalog{definitions: defs}, warnings
}

// entryIsDir reports whether entry (or, if entry is a symlink, its target)
// is a directory.
func entryIsDir(root string, entry os.DirEntry) bool {
	if entry.IsDir() {
		return true
	}
	if entry.Type()&os.ModeSymlink == 0 {
		return false
	}
	info, err := os.Stat(filepath.Join(root, entry.Name()))
	return err == nil && info.IsDir()
}

// loadCandidate parses and validates one agent directory. On success it
// returns the Definition with an empty warning; on failure it returns a
// zero Definition and a non-empty warning string of the form "<path>:
// <reason>".
func loadCandidate(dir, agentPath, dirName string) (Definition, string) {
	data, err := os.ReadFile(agentPath)
	if err != nil {
		return Definition{}, fmt.Sprintf("%s: %s", agentPath, err)
	}
	fields, body, err := skill.ParseFrontmatter(data)
	if err != nil {
		return Definition{}, fmt.Sprintf("%s: %s", agentPath, err)
	}
	name, err := validateAgentName(fields, dirName)
	if err != nil {
		return Definition{}, fmt.Sprintf("%s: %s", agentPath, err)
	}
	description, err := validateAgentDescription(fields)
	if err != nil {
		return Definition{}, fmt.Sprintf("%s: %s", agentPath, err)
	}
	tools, err := parseAgentTools(fields)
	if err != nil {
		return Definition{}, fmt.Sprintf("%s: %s", agentPath, err)
	}
	context, err := validateAgentContext(fields)
	if err != nil {
		return Definition{}, fmt.Sprintf("%s: %s", agentPath, err)
	}
	model := strings.TrimSpace(fields["model"])

	return Definition{
		Name:        name,
		Description: description,
		Tools:       tools,
		Model:       model,
		Context:     context,
		Body:        strings.TrimSpace(body),
		Dir:         dir,
		Path:        agentPath,
	}, ""
}

func validateAgentName(fields map[string]string, dirName string) (string, error) {
	raw, ok := fields["name"]
	if !ok || raw == "" {
		return "", errors.New("missing name")
	}
	if len(raw) > maxAgentNameLength || !agentNamePattern.MatchString(raw) {
		return "", fmt.Errorf("name %q is invalid", raw)
	}
	if raw != dirName {
		return "", fmt.Errorf("name %q does not match directory %q", raw, dirName)
	}
	return raw, nil
}

func validateAgentDescription(fields map[string]string) (string, error) {
	trimmed := strings.TrimSpace(fields["description"])
	if trimmed == "" {
		return "", errors.New("missing description")
	}
	if utf8.RuneCountInString(trimmed) > maxAgentDescriptionRunes {
		return "", fmt.Errorf("description exceeds %d characters", maxAgentDescriptionRunes)
	}
	return trimmed, nil
}

// parseAgentTools parses the optional tools frontmatter key. Absent key ->
// nil, nil (meaning "all child tools"). Present but empty after splitting ->
// an error naming the required comma-separated-list format. Any item that
// does not match ^[a-z0-9_]+$ -> an error naming that item.
func parseAgentTools(fields map[string]string) ([]string, error) {
	raw, ok := fields["tools"]
	if !ok {
		return nil, nil
	}
	var tools []string
	for _, item := range strings.Split(raw, ",") {
		item = strings.TrimSpace(item)
		if item == "" {
			continue
		}
		if !agentToolNamePattern.MatchString(item) {
			return nil, fmt.Errorf("tools item %q is invalid", item)
		}
		tools = append(tools, item)
	}
	if len(tools) == 0 {
		return nil, errors.New("tools must be a comma-separated list")
	}
	return tools, nil
}

func validateAgentContext(fields map[string]string) (string, error) {
	raw, ok := fields["context"]
	if !ok || strings.TrimSpace(raw) == "" {
		return "fresh", nil
	}
	trimmed := strings.TrimSpace(raw)
	if trimmed != "fresh" && trimmed != "inherit" {
		return "", errors.New(`context must be "fresh" or "inherit"`)
	}
	return trimmed, nil
}
