package config

import "fmt"

// defaultAgentsPaths is used when [agents].paths is absent from the config
// file. A configured empty slice (paths = []) means no roots, and is left
// nil by the TOML decoder's zero value vs. an explicit []string{}.
var defaultAgentsPaths = []string{"~/.otto/agents", ".otto/agents"}

const defaultAgentsMaxParallel = 4

type Agents struct {
	Enabled     *bool    `toml:"enabled"`
	Paths       []string `toml:"paths"`
	MaxParallel *int     `toml:"max_parallel"`
}

// AgentsRuntime is the resolved [agents] configuration for one runner build.
type AgentsRuntime struct {
	Enabled bool
	// Roots are absolute, cleaned agent root directories in configured
	// order. Later entries win on a name conflict during discovery.
	Roots       []string
	MaxParallel int // 1..16, default 4
}

// ResolveAgents resolves [agents] into roots ready for subagent.Discover.
// MaxParallel is validated before the enabled short-circuit, so an
// out-of-range value is always reported even when agents are disabled. An
// unresolvable "~/" entry (no home in env) is skipped, same as
// ResolveSkills.
func ResolveAgents(file File, env map[string]string, workspacePath string) (AgentsRuntime, error) {
	maxParallel := defaultAgentsMaxParallel
	if file.Agents.MaxParallel != nil {
		maxParallel = *file.Agents.MaxParallel
	}
	if maxParallel < 1 || maxParallel > 16 {
		return AgentsRuntime{}, fmt.Errorf("[agents].max_parallel must be between 1 and 16, got %d", maxParallel)
	}

	enabled := true
	if file.Agents.Enabled != nil {
		enabled = *file.Agents.Enabled
	}
	if !enabled {
		return AgentsRuntime{Enabled: false, MaxParallel: maxParallel}, nil
	}

	paths := file.Agents.Paths
	if paths == nil {
		paths = defaultAgentsPaths
	}

	return AgentsRuntime{Enabled: true, Roots: resolveRoots(paths, env, workspacePath), MaxParallel: maxParallel}, nil
}
