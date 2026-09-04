// Package skill discovers and loads Otto skills: directories containing a
// SKILL.md file with YAML frontmatter, following the Agent Skills format.
package skill

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
)

// Skill is one discovered skill.
type Skill struct {
	// Name is the frontmatter name, equal to the directory base name.
	Name string
	// Description is the frontmatter description, trimmed.
	Description string
	// Dir is the absolute skill directory.
	Dir string
	// Path is filepath.Join(Dir, "SKILL.md").
	Path string
}

// Catalog is the set of skills discovered for one runner. The zero value is
// empty and usable.
type Catalog struct {
	skills []Skill // sorted by Name
}

// Skills returns the catalog's skills sorted by name. The returned slice is
// a copy.
func (c Catalog) Skills() []Skill {
	out := make([]Skill, len(c.skills))
	copy(out, c.skills)
	return out
}

// Lookup returns the skill with the given name, if any.
func (c Catalog) Lookup(name string) (Skill, bool) {
	for _, s := range c.skills {
		if s.Name == name {
			return s, true
		}
	}
	return Skill{}, false
}

// Len returns the number of skills in the catalog.
func (c Catalog) Len() int {
	return len(c.skills)
}

var skillNamePattern = regexp.MustCompile(`^[a-z0-9]+(-[a-z0-9]+)*$`)

const maxSkillNameLength = 64
const maxSkillDescriptionRunes = 1024

// Discover scans roots in order; a later root overrides an earlier one on
// the same name. roots are absolute directories. A missing root is skipped
// silently. An unreadable root or an invalid skill directory produces one
// warning string and is skipped. Discover never errors.
func Discover(roots []string) (Catalog, []string) {
	var warnings []string
	byName := make(map[string]Skill)

	for _, root := range roots {
		entries, err := os.ReadDir(root)
		if err != nil {
			if errors.Is(err, fs.ErrNotExist) {
				continue
			}
			warnings = append(warnings, fmt.Sprintf("skills root %s: %s", root, err))
			continue
		}
		for _, entry := range entries {
			if !entryIsDir(root, entry) {
				continue
			}
			dir := filepath.Join(root, entry.Name())
			skillPath := filepath.Join(dir, "SKILL.md")
			info, err := os.Stat(skillPath)
			if err != nil || !info.Mode().IsRegular() {
				// A directory without SKILL.md is silently ignored.
				continue
			}
			skill, warning := loadCandidate(dir, skillPath, entry.Name())
			if warning != "" {
				warnings = append(warnings, warning)
				continue
			}
			byName[skill.Name] = skill
		}
	}

	names := make([]string, 0, len(byName))
	for name := range byName {
		names = append(names, name)
	}
	sort.Strings(names)
	skills := make([]Skill, len(names))
	for i, name := range names {
		skills[i] = byName[name]
	}
	return Catalog{skills: skills}, warnings
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

// loadCandidate parses and validates one skill directory. On success it
// returns the Skill with an empty warning; on failure it returns a zero
// Skill and a non-empty warning string.
func loadCandidate(dir, skillPath, dirName string) (Skill, string) {
	data, err := os.ReadFile(skillPath)
	if err != nil {
		return Skill{}, fmt.Sprintf("skill %s: %s", skillPath, err)
	}
	fields, _, err := ParseFrontmatter(data)
	if err != nil {
		return Skill{}, fmt.Sprintf("skill %s: %s", skillPath, err)
	}
	name, err := validateSkillName(fields, dirName)
	if err != nil {
		return Skill{}, fmt.Sprintf("skill %s: %s", skillPath, err)
	}
	description, err := validateSkillDescription(fields)
	if err != nil {
		return Skill{}, fmt.Sprintf("skill %s: %s", skillPath, err)
	}
	return Skill{Name: name, Description: description, Dir: dir, Path: skillPath}, ""
}

func validateSkillName(fields map[string]string, dirName string) (string, error) {
	raw, ok := fields["name"]
	if !ok || raw == "" {
		return "", errors.New("missing name")
	}
	if len(raw) > maxSkillNameLength || !skillNamePattern.MatchString(raw) {
		return "", fmt.Errorf("name %q is invalid", raw)
	}
	if raw != dirName {
		return "", fmt.Errorf("name %q does not match directory %q", raw, dirName)
	}
	return raw, nil
}

func validateSkillDescription(fields map[string]string) (string, error) {
	trimmed := strings.TrimSpace(fields["description"])
	if trimmed == "" {
		return "", errors.New("missing description")
	}
	if utf8.RuneCountInString(trimmed) > maxSkillDescriptionRunes {
		return "", fmt.Errorf("description exceeds %d characters", maxSkillDescriptionRunes)
	}
	return trimmed, nil
}

// Load reads s.Path and returns the Markdown body without the frontmatter.
func Load(s Skill) (string, error) {
	data, err := os.ReadFile(s.Path)
	if err != nil {
		return "", err
	}
	_, body, err := ParseFrontmatter(data)
	if err != nil {
		return "", err
	}
	return body, nil
}

// ListFiles returns relative slash-separated paths of regular files under
// dir, recursively, excluding SKILL.md at the top level, skipping hidden
// names (leading "."), not following symlinks (a symlink is neither listed
// nor descended), sorted, at most limit entries. total is the count before
// applying limit.
func ListFiles(dir string, limit int) (files []string, total int, err error) {
	err = filepath.WalkDir(dir, func(path string, d fs.DirEntry, walkErr error) error {
		if walkErr != nil {
			return walkErr
		}
		if path == dir {
			return nil
		}
		name := d.Name()
		if strings.HasPrefix(name, ".") {
			if d.IsDir() {
				return filepath.SkipDir
			}
			return nil
		}
		if d.Type()&os.ModeSymlink != 0 {
			return nil
		}
		if d.IsDir() {
			return nil
		}
		if !d.Type().IsRegular() {
			return nil
		}
		rel, relErr := filepath.Rel(dir, path)
		if relErr != nil {
			return relErr
		}
		rel = filepath.ToSlash(rel)
		if rel == "SKILL.md" {
			return nil
		}
		files = append(files, rel)
		return nil
	})
	if err != nil {
		return nil, 0, err
	}
	sort.Strings(files)
	total = len(files)
	if limit > 0 && total > limit {
		files = files[:limit]
	}
	return files, total, nil
}
