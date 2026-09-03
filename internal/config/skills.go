package config

import (
	"path/filepath"
	"strings"
)

// defaultSkillsPaths is used when [skills].paths is absent from the config
// file. A configured empty slice (paths = []) means no roots, and is left
// nil by the TOML decoder's zero value vs. an explicit []string{}.
var defaultSkillsPaths = []string{"~/.otto/skills", ".otto/skills"}

type Skills struct {
	Enabled *bool    `toml:"enabled"`
	Paths   []string `toml:"paths"`
}

// SkillsRuntime is the resolved [skills] configuration for one runner build.
type SkillsRuntime struct {
	Enabled bool
	// Roots are absolute, cleaned skill root directories in configured
	// order. Later entries win on a name conflict during discovery.
	Roots []string
}

// ResolveSkills resolves [skills] into roots ready for skill.Discover. It
// never errors: an unresolvable "~/" entry (no home in env) is skipped.
func ResolveSkills(file File, env map[string]string, workspacePath string) SkillsRuntime {
	enabled := true
	if file.Skills.Enabled != nil {
		enabled = *file.Skills.Enabled
	}
	if !enabled {
		return SkillsRuntime{Enabled: false}
	}

	paths := file.Skills.Paths
	if paths == nil {
		paths = defaultSkillsPaths
	}

	var roots []string
	home := homeFromEnv(env)
	for _, path := range paths {
		switch {
		case path == "":
			continue
		case strings.HasPrefix(path, "~/"):
			if home == "" {
				continue
			}
			roots = append(roots, filepath.Clean(filepath.Join(home, strings.TrimPrefix(path, "~/"))))
		case filepath.IsAbs(path):
			roots = append(roots, filepath.Clean(path))
		default:
			roots = append(roots, filepath.Clean(filepath.Join(workspacePath, path)))
		}
	}
	return SkillsRuntime{Enabled: true, Roots: roots}
}
