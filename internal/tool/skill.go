package tool

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"strings"

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/skill"
)

const maxSkillFileListing = 50

type skillTool struct {
	catalog        skill.Catalog
	maxOutputBytes int
}

type skillToolArgs struct {
	Name string `json:"name"`
	File string `json:"file,omitempty"`
}

// NewSkillTool returns the "skill" tool: it loads a skill's instructions by
// name, or reads a file inside that skill's directory.
func NewSkillTool(catalog skill.Catalog, maxOutputBytes int) Tool {
	return &skillTool{catalog: catalog, maxOutputBytes: maxOutputBytes}
}

func (t *skillTool) Definition() model.ToolDefinition {
	return model.ToolDefinition{
		Name:        "skill",
		Description: "Load a skill's instructions by name, or read a file inside that skill's directory. Call it before starting a task that matches a listed skill.",
		Parameters: map[string]any{
			"type":                 "object",
			"additionalProperties": false,
			"properties": map[string]any{
				"name": map[string]any{
					"type":        "string",
					"description": "Skill name from the available_skills listing",
				},
				"file": map[string]any{
					"type":        "string",
					"description": "Optional relative path of a file inside the skill directory",
				},
			},
			"required": []string{"name"},
		},
	}
}

func (t *skillTool) Execute(ctx context.Context, arguments json.RawMessage) Result {
	var args skillToolArgs
	if err := DecodeStrictJSON(arguments, &args, "name"); err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	if err := ctx.Err(); err != nil {
		return Result{Content: err.Error(), IsError: true}
	}

	s, ok := t.catalog.Lookup(args.Name)
	if !ok {
		return Result{Content: fmt.Sprintf("unknown skill: %s", args.Name), IsError: true}
	}

	if args.File == "" {
		return t.loadSkill(s)
	}
	return t.readSkillFile(s, args.File)
}

func (t *skillTool) loadSkill(s skill.Skill) Result {
	body, err := skill.Load(s)
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	files, total, err := skill.ListFiles(s.Dir, maxSkillFileListing)
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	content := fmt.Sprintf("skill: %s\nlocation: %s\nfiles: %s\n\n%s",
		s.Name, s.Dir, formatSkillFileListing(files, total), body)
	return CappedTextResult(content, t.maxOutputBytes)
}

func formatSkillFileListing(files []string, total int) string {
	if total == 0 {
		return "none"
	}
	listing := strings.Join(files, ", ")
	if total > len(files) {
		listing += fmt.Sprintf(", ... (%d files)", total)
	}
	return listing
}

func (t *skillTool) readSkillFile(s skill.Skill, file string) Result {
	workspace, err := NewWorkspace(s.Dir)
	if err != nil {
		return Result{Content: fmt.Sprintf("skill %s: %s", s.Name, err), IsError: true}
	}
	defer workspace.Close()
	opened, err := workspace.Open(file)
	if err != nil {
		return Result{Content: fmt.Sprintf("skill %s: %s", s.Name, err), IsError: true}
	}
	defer opened.Close()
	info, err := opened.Stat()
	if err != nil {
		return Result{Content: fmt.Sprintf("skill %s: %s", s.Name, err), IsError: true}
	}
	if !info.Mode().IsRegular() {
		return Result{Content: fmt.Sprintf("not a regular file: %s", file), IsError: true}
	}
	if info.Size() > maxReadFileBytes {
		return Result{Content: fmt.Sprintf("file is too large (%d bytes); maximum readable size is %d bytes", info.Size(), maxReadFileBytes), IsError: true}
	}
	data, err := io.ReadAll(io.LimitReader(opened, maxReadFileBytes+1))
	if err != nil {
		return Result{Content: fmt.Sprintf("skill %s: %s", s.Name, err), IsError: true}
	}
	if len(data) > maxReadFileBytes {
		return Result{Content: fmt.Sprintf("file is too large (%d bytes); maximum readable size is %d bytes", len(data), maxReadFileBytes), IsError: true}
	}
	return CappedTextResult(string(data), t.maxOutputBytes)
}

var _ Tool = (*skillTool)(nil)
