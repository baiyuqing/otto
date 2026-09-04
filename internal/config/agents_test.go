package config

import (
	"path/filepath"
	"reflect"
	"testing"
)

func TestResolveAgentsDefaults(t *testing.T) {
	home := filepath.Join(t.TempDir(), "home")
	workspace := filepath.Join(t.TempDir(), "workspace")

	runtime, err := ResolveAgents(File{}, map[string]string{"HOME": home}, workspace)
	if err != nil {
		t.Fatal(err)
	}

	want := AgentsRuntime{
		Enabled:     true,
		Roots:       []string{filepath.Join(home, ".otto", "agents"), filepath.Join(workspace, ".otto", "agents")},
		MaxParallel: defaultAgentsMaxParallel,
	}
	if !reflect.DeepEqual(runtime, want) {
		t.Fatalf("ResolveAgents() = %+v, want %+v", runtime, want)
	}
}

func TestResolveAgentsDisabledYieldsNoRoots(t *testing.T) {
	home := filepath.Join(t.TempDir(), "home")
	workspace := filepath.Join(t.TempDir(), "workspace")
	disabled := false
	file := File{Agents: Agents{Enabled: &disabled}}

	runtime, err := ResolveAgents(file, map[string]string{"HOME": home}, workspace)
	if err != nil {
		t.Fatal(err)
	}

	if runtime.Enabled {
		t.Fatalf("Enabled = true, want false")
	}
	if runtime.Roots != nil {
		t.Fatalf("Roots = %#v, want nil when disabled", runtime.Roots)
	}
	if runtime.MaxParallel != defaultAgentsMaxParallel {
		t.Fatalf("MaxParallel = %d, want %d even when disabled", runtime.MaxParallel, defaultAgentsMaxParallel)
	}
}

func TestResolveAgentsExplicitEmptyPathsMeansNoRoots(t *testing.T) {
	home := filepath.Join(t.TempDir(), "home")
	workspace := filepath.Join(t.TempDir(), "workspace")
	file := File{Agents: Agents{Paths: []string{}}}

	runtime, err := ResolveAgents(file, map[string]string{"HOME": home}, workspace)
	if err != nil {
		t.Fatal(err)
	}

	if runtime.Roots != nil {
		t.Fatalf("Roots = %#v, want nil for explicit empty paths", runtime.Roots)
	}
	if !runtime.Enabled {
		t.Fatalf("Enabled = false, want true")
	}
}

func TestResolveAgentsExpandsHomeRelativeAndAbsolutePaths(t *testing.T) {
	home := filepath.Join(t.TempDir(), "home")
	workspace := filepath.Join(t.TempDir(), "workspace")
	file := File{Agents: Agents{Paths: []string{"~/agents-a", "relative/agents-b", "/abs/agents-c"}}}

	runtime, err := ResolveAgents(file, map[string]string{"HOME": home}, workspace)
	if err != nil {
		t.Fatal(err)
	}

	want := []string{
		filepath.Join(home, "agents-a"),
		filepath.Join(workspace, "relative/agents-b"),
		filepath.Clean("/abs/agents-c"),
	}
	if !reflect.DeepEqual(runtime.Roots, want) {
		t.Fatalf("Roots = %#v, want %#v", runtime.Roots, want)
	}
}

func TestResolveAgentsSkipsHomeTildeWithoutHome(t *testing.T) {
	t.Setenv("HOME", "")
	workspace := filepath.Join(t.TempDir(), "workspace")
	file := File{Agents: Agents{Paths: []string{"", "~/agents-a", "relative/agents-b"}}}

	runtime, err := ResolveAgents(file, map[string]string{"HOME": ""}, workspace)
	if err != nil {
		t.Fatal(err)
	}

	want := []string{filepath.Join(workspace, "relative/agents-b")}
	if !reflect.DeepEqual(runtime.Roots, want) {
		t.Fatalf("Roots = %#v, want %#v", runtime.Roots, want)
	}
}

func TestResolveAgentsMaxParallelBoundaries(t *testing.T) {
	home := filepath.Join(t.TempDir(), "home")
	workspace := filepath.Join(t.TempDir(), "workspace")

	for _, value := range []int{1, 16} {
		value := value
		file := File{Agents: Agents{MaxParallel: &value}}
		runtime, err := ResolveAgents(file, map[string]string{"HOME": home}, workspace)
		if err != nil {
			t.Fatalf("max_parallel = %d: unexpected error: %v", value, err)
		}
		if runtime.MaxParallel != value {
			t.Fatalf("max_parallel = %d: MaxParallel = %d, want %d", value, runtime.MaxParallel, value)
		}
	}

	for _, value := range []int{0, 17} {
		value := value
		file := File{Agents: Agents{MaxParallel: &value}}
		_, err := ResolveAgents(file, map[string]string{"HOME": home}, workspace)
		if err == nil {
			t.Fatalf("max_parallel = %d: expected error, got nil", value)
		}
	}
}

func TestResolveAgentsMaxParallelErrorMessage(t *testing.T) {
	home := filepath.Join(t.TempDir(), "home")
	workspace := filepath.Join(t.TempDir(), "workspace")
	value := 0
	file := File{Agents: Agents{MaxParallel: &value}}

	_, err := ResolveAgents(file, map[string]string{"HOME": home}, workspace)
	if err == nil {
		t.Fatal("expected error, got nil")
	}
	want := "[agents].max_parallel must be between 1 and 16, got 0"
	if err.Error() != want {
		t.Fatalf("err = %q, want %q", err.Error(), want)
	}
}

func TestResolveAgentsMaxParallelRejectedEvenWhenDisabled(t *testing.T) {
	home := filepath.Join(t.TempDir(), "home")
	workspace := filepath.Join(t.TempDir(), "workspace")
	disabled := false
	value := 17
	file := File{Agents: Agents{Enabled: &disabled, MaxParallel: &value}}

	_, err := ResolveAgents(file, map[string]string{"HOME": home}, workspace)
	if err == nil {
		t.Fatal("expected error, got nil even though disabled")
	}
	want := "[agents].max_parallel must be between 1 and 16, got 17"
	if err.Error() != want {
		t.Fatalf("err = %q, want %q", err.Error(), want)
	}
}

func TestLoadRequiredDecodesAgentsSection(t *testing.T) {
	path := writeConfig(t, `[agents]
enabled = false
paths = ["~/.otto/agents", ".otto/agents"]
max_parallel = 8
`)
	file, err := LoadRequired(path)
	if err != nil {
		t.Fatal(err)
	}
	if file.Agents.Enabled == nil || *file.Agents.Enabled != false {
		t.Fatalf("Agents.Enabled = %#v, want explicit false", file.Agents.Enabled)
	}
	if got := file.Agents.Paths; !reflect.DeepEqual(got, []string{"~/.otto/agents", ".otto/agents"}) {
		t.Fatalf("Agents.Paths = %#v", got)
	}
	if file.Agents.MaxParallel == nil || *file.Agents.MaxParallel != 8 {
		t.Fatalf("Agents.MaxParallel = %#v, want pointer to 8", file.Agents.MaxParallel)
	}
}
