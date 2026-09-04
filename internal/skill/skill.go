// Package skill discovers and loads Otto skills: directories containing a
// SKILL.md file with YAML frontmatter, following the Agent Skills format.
package skill

import (
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"syscall"
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
const maxSkillFileBytes = 64 << 20

// Discover scans roots in order; a later root overrides an earlier one on
// the same name. roots are absolute directories. A missing root is skipped
// silently. An unreadable root or an invalid skill directory produces one
// warning string and is skipped. Discover never errors.
func Discover(roots []string) (Catalog, []string) {
	var warnings []string
	byName := make(map[string]Skill)

	for _, root := range roots {
		rootFS, err := os.OpenRoot(root)
		if err != nil {
			if errors.Is(err, fs.ErrNotExist) {
				continue
			}
			warnings = append(warnings, fmt.Sprintf("skills root %s: %s", root, err))
			continue
		}
		canonicalRoot, err := filepath.EvalSymlinks(root)
		if err != nil {
			_ = rootFS.Close()
			warnings = append(warnings, fmt.Sprintf("skills root %s: %s", root, err))
			continue
		}
		entries, err := fs.ReadDir(rootFS.FS(), ".")
		if err != nil {
			_ = rootFS.Close()
			warnings = append(warnings, fmt.Sprintf("skills root %s: %s", root, err))
			continue
		}
		for _, entry := range entries {
			dirFS, ok := openSkillDir(rootFS, canonicalRoot, entry)
			if !ok {
				continue
			}
			dir := filepath.Join(root, entry.Name())
			skillPath := filepath.Join(dir, "SKILL.md")
			info, err := dirFS.Stat("SKILL.md")
			if err != nil || !info.Mode().IsRegular() {
				_ = dirFS.Close()
				if err != nil && !errors.Is(err, fs.ErrNotExist) {
					warnings = append(warnings, fmt.Sprintf("skill %s: %s", skillPath, err))
				}
				// A directory without SKILL.md is silently ignored.
				continue
			}
			skill, warning := loadCandidate(dir, skillPath, entry.Name(), dirFS)
			_ = dirFS.Close()
			if warning != "" {
				warnings = append(warnings, warning)
				continue
			}
			byName[skill.Name] = skill
		}
		_ = rootFS.Close()
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

func openSkillDir(rootFS *os.Root, root string, entry os.DirEntry) (*os.Root, bool) {
	if entry.IsDir() {
		dirFS, err := rootFS.OpenRoot(entry.Name())
		return dirFS, err == nil
	}
	if entry.Type()&os.ModeSymlink == 0 {
		return nil, false
	}
	safeDir, err := filepath.EvalSymlinks(filepath.Join(root, entry.Name()))
	if err != nil {
		return nil, false
	}
	dirFS, err := os.OpenRoot(safeDir)
	if err != nil {
		return nil, false
	}
	info, err := dirFS.Stat(".")
	if err != nil || !info.IsDir() {
		_ = dirFS.Close()
		return nil, false
	}
	return dirFS, true
}

// loadCandidate parses and validates one skill directory. On success it
// returns the Skill with an empty warning; on failure it returns a zero
// Skill and a non-empty warning string.
func loadCandidate(dir, skillPath, dirName string, root *os.Root) (Skill, string) {
	data, err := readRootFile(root, "SKILL.md")
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
	name, err := filepath.Rel(s.Dir, s.Path)
	if err != nil {
		return "", err
	}
	root, err := os.OpenRoot(s.Dir)
	if err != nil {
		return "", err
	}
	defer root.Close()
	data, err := readRootFile(root, name)
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
	root, err := os.OpenRoot(dir)
	if err != nil {
		return nil, 0, err
	}
	defer root.Close()
	err = fs.WalkDir(root.FS(), ".", func(path string, d fs.DirEntry, walkErr error) error {
		if walkErr != nil {
			return walkErr
		}
		if path == "." {
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
		rel := filepath.ToSlash(path)
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

func readRootFile(root *os.Root, name string) ([]byte, error) {
	file, err := root.OpenFile(name, os.O_RDONLY|syscall.O_NONBLOCK, 0)
	if err != nil {
		return nil, err
	}
	defer file.Close()
	info, err := file.Stat()
	if err != nil {
		return nil, err
	}
	if !info.Mode().IsRegular() {
		return nil, fmt.Errorf("not a regular file: %s", name)
	}
	if info.Size() > maxSkillFileBytes {
		return nil, fmt.Errorf("file is too large (%d bytes); maximum readable size is %d bytes", info.Size(), maxSkillFileBytes)
	}
	data, err := io.ReadAll(io.LimitReader(file, maxSkillFileBytes+1))
	if err != nil {
		return nil, err
	}
	if len(data) > maxSkillFileBytes {
		return nil, fmt.Errorf("file is too large (%d bytes); maximum readable size is %d bytes", len(data), maxSkillFileBytes)
	}
	return data, nil
}
