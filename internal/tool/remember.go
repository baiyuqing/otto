package tool

import (
	"context"
	"encoding/json"
	"fmt"

	"github.com/baiyuqing/otto/internal/memory"
	"github.com/baiyuqing/otto/internal/model"
)

type rememberTool struct {
	proposer     memory.Proposer
	defaultScope memory.Scope
}

type rememberArgs struct {
	Kind         string   `json:"kind"`
	Key          string   `json:"key,omitempty"`
	Text         string   `json:"text"`
	Labels       []string `json:"labels,omitempty"`
	Confidence   float64  `json:"confidence,omitempty"`
	Reason       string   `json:"reason,omitempty"`
	TargetID     string   `json:"target_id,omitempty"`
	BaseRevision uint64   `json:"base_revision,omitempty"`
}

func NewRememberTool(proposer memory.Proposer, defaultScope memory.Scope) Tool {
	return &rememberTool{proposer: proposer, defaultScope: defaultScope}
}

func (t *rememberTool) Definition() model.ToolDefinition {
	return model.ToolDefinition{
		Name:        "remember",
		Description: "Propose a fact or preference to remember; a human reviews it before it takes effect",
		Parameters: map[string]any{
			"type":                 "object",
			"additionalProperties": false,
			"properties": map[string]any{
				"kind": map[string]any{"type": "string", "description": "Record category, e.g. preference, instruction, fact"},
				"key":  map[string]any{"type": "string", "description": "Stable key for updating this record later"},
				"text": map[string]any{"type": "string", "description": "Text to remember"},
				"labels": map[string]any{
					"type":  "array",
					"items": map[string]any{"type": "string"},
				},
				"confidence":    map[string]any{"type": "number", "description": "Confidence from 0 to 1"},
				"reason":        map[string]any{"type": "string", "description": "Why this should be remembered"},
				"target_id":     map[string]any{"type": "string", "description": "Existing record ID, from memory_search, when proposing an update"},
				"base_revision": map[string]any{"type": "integer", "description": "Revision of the record being updated, from memory_search"},
			},
			"required": []string{"kind", "text"},
		},
	}
}

func (t *rememberTool) Execute(ctx context.Context, arguments json.RawMessage) Result {
	var args rememberArgs
	if err := DecodeStrictJSON(arguments, &args, "kind", "text"); err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	if err := ctx.Err(); err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	action := memory.CandidateCreate
	if args.TargetID != "" {
		action = memory.CandidateUpdate
	}
	batch, err := t.proposer.Propose(ctx, memory.ProposeRequest{
		Action:       action,
		Scope:        t.defaultScope,
		Kind:         args.Kind,
		Key:          args.Key,
		Text:         args.Text,
		Labels:       args.Labels,
		Confidence:   args.Confidence,
		TargetID:     args.TargetID,
		BaseRevision: args.BaseRevision,
		Reason:       args.Reason,
		Source:       memory.Provenance{Origin: memory.OriginModel},
	})
	if err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	if len(batch.Candidates) == 0 {
		return Result{Content: "proposal was not queued for review"}
	}
	candidate := batch.Candidates[0]
	return Result{Content: fmt.Sprintf("candidate %s queued for human review (state=%s)", candidate.ID, candidate.State)}
}

var _ Tool = (*rememberTool)(nil)
