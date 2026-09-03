package config

import (
	"path/filepath"
	"reflect"
	"testing"
)

func TestResolveSkillsDefaults(t *testing.T) {
	home := filepath.Join(t.TempDir(), "home")
	workspace := filepath.Join(t.TempDir(), "workspace")

	runtime := ResolveSkills(File{}, map[string]string{"HOME": home}, workspace)

	want := SkillsRuntime{
		Enabled: true,
		Roots:   []string{filepath.Join(home, ".otto", "skills"), filepath.Join(workspace, ".otto", "skills")},
	}
	if !reflect.DeepEqual(runtime, want) {
		t.Fatalf("ResolveSkills() = %+v, want %+v", runtime, want)
	}
}

func TestResolveSkillsExpandsHomeRelativeAndAbsolutePaths(t *testing.T) {
	home := filepath.Join(t.TempDir(), "home")
	workspace := filepath.Join(t.TempDir(), "workspace")
	file := File{Skills: Skills{Paths: []string{"~/skills-a", "relative/skills-b", "/abs/skills-c"}}}

	runtime := ResolveSkills(file, map[string]string{"HOME": home}, workspace)

	want := []string{
		filepath.Join(home, "skills-a"),
		filepath.Join(workspace, "relative/skills-b"),
		filepath.Clean("/abs/skills-c"),
	}
	if !reflect.DeepEqual(runtime.Roots, want) {
		t.Fatalf("Roots = %#v, want %#v", runtime.Roots, want)
	}
}

func TestResolveSkillsEmptyPathsMeansNoRoots(t *testing.T) {
	home := filepath.Join(t.TempDir(), "home")
	workspace := filepath.Join(t.TempDir(), "workspace")
	file := File{Skills: Skills{Paths: []string{}}}

	runtime := ResolveSkills(file, map[string]string{"HOME": home}, workspace)

	if runtime.Roots != nil {
		t.Fatalf("Roots = %#v, want nil for explicit empty paths", runtime.Roots)
	}
	if !runtime.Enabled {
		t.Fatalf("Enabled = false, want true")
	}
}

func TestResolveSkillsDisabledYieldsNoRoots(t *testing.T) {
	home := filepath.Join(t.TempDir(), "home")
	workspace := filepath.Join(t.TempDir(), "workspace")
	disabled := false
	file := File{Skills: Skills{Enabled: &disabled}}

	runtime := ResolveSkills(file, map[string]string{"HOME": home}, workspace)

	if runtime.Enabled {
		t.Fatalf("Enabled = true, want false")
	}
	if runtime.Roots != nil {
		t.Fatalf("Roots = %#v, want nil when disabled", runtime.Roots)
	}
}

func TestResolveSkillsSkipsEmptyEntryAndHomeTildeWithoutHome(t *testing.T) {
	t.Setenv("HOME", "")
	workspace := filepath.Join(t.TempDir(), "workspace")
	file := File{Skills: Skills{Paths: []string{"", "~/skills-a", "relative/skills-b"}}}

	runtime := ResolveSkills(file, map[string]string{"HOME": ""}, workspace)

	want := []string{filepath.Join(workspace, "relative/skills-b")}
	if !reflect.DeepEqual(runtime.Roots, want) {
		t.Fatalf("Roots = %#v, want %#v", runtime.Roots, want)
	}
}

func TestLoadRequiredDecodesSkillsSection(t *testing.T) {
	path := writeConfig(t, `[skills]
enabled = false
paths = ["~/.otto/skills", ".otto/skills"]
`)
	file, err := LoadRequired(path)
	if err != nil {
		t.Fatal(err)
	}
	if file.Skills.Enabled == nil || *file.Skills.Enabled != false {
		t.Fatalf("Skills.Enabled = %#v, want explicit false", file.Skills.Enabled)
	}
	if got := file.Skills.Paths; !reflect.DeepEqual(got, []string{"~/.otto/skills", ".otto/skills"}) {
		t.Fatalf("Skills.Paths = %#v", got)
	}
}
