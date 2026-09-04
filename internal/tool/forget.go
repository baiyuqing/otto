package tool

import (
	"context"
	"encoding/json"
	"fmt"

	"github.com/baiyuqing/otto/internal/memory"
	"github.com/baiyuqing/otto/internal/model"
)

type forgetTool struct {
	proposer memory.Proposer
	scopes   []memory.Scope
}

type forgetArgs struct {
	ScopeNamespace string `json:"scope_namespace"`
	ScopeID        string `json:"scope_id"`
	ID             string `json:"id"`
	Revision       uint64 `json:"revision"`
	Reason         string `json:"reason,omitempty"`
}

func NewForgetTool(proposer memory.Proposer, scopes []memory.Scope) Tool {
	return &forgetTool{proposer: proposer, scopes: scopes}
}

func (t *forgetTool) Definition() model.ToolDefinition {
	return model.ToolDefinition{
		Name:        "forget",
		Description: "Propose forgetting a remembered record found via memory_search; a human reviews it before it takes effect",
		Parameters: map[string]any{
			"type":                 "object",
			"additionalProperties": false,
			"properties": map[string]any{
				"scope_namespace": map[string]any{"type": "string", "description": "Scope namespace of the record, from memory_search"},
				"scope_id":        map[string]any{"type": "string", "description": "Scope ID of the record, from memory_search"},
				"id":              map[string]any{"type": "string", "description": "Record ID to forget"},
				"revision":        map[string]any{"type": "integer", "description": "Record revision, from memory_search"},
				"reason":          map[string]any{"type": "string", "description": "Why this should be forgotten"},
			},
			"required": []string{"scope_namespace", "scope_id", "id", "revision"},
		},
	}
}

func (t *forgetTool) Execute(ctx context.Context, arguments json.RawMessage) Result {
	var args forgetArgs
	if err := DecodeStrictJSON(arguments, &args, "scope_namespace", "scope_id", "id", "revision"); err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	if err := ctx.Err(); err != nil {
		return Result{Content: err.Error(), IsError: true}
	}
	scope := memory.Scope{Namespace: args.ScopeNamespace, ID: args.ScopeID}
	if !scopeBound(t.scopes, scope) {
		return Result{Content: "scope is not one of the scopes this session is bound to", IsError: true}
	}
	batch, err := t.proposer.Propose(ctx, memory.ProposeRequest{
		Action:       memory.CandidateForget,
		Scope:        scope,
		TargetID:     args.ID,
		BaseRevision: args.Revision,
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

func scopeBound(bound []memory.Scope, scope memory.Scope) bool {
	for _, candidate := range bound {
		if candidate == scope {
			return true
		}
	}
	return false
}

var _ Tool = (*forgetTool)(nil)
