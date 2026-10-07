package agent

import (
	"context"
	"encoding/json"
	"errors"
)

var ErrApprovalPolicyUnsupported = errors.New("agent does not support approval policy changes; upgrade Otto")

// ApprovalPolicy is the canonical session policy, with legacy projections.
type ApprovalPolicy struct {
	Policy         string `json:"policy"`
	Reviewer       string `json:"reviewer"`
	Revision       uint64 `json:"revision"`
	Mode           string `json:"mode"`
	Automatic      bool   `json:"automatic"`
	ReviewerActive bool   `json:"reviewer_active"`
	Legacy         bool   `json:"-"`
}

func (a *Agent) ApprovalPolicy(ctx context.Context, sid string) (*ApprovalPolicy, error) {
	p, err := a.process(ctx)
	if err != nil {
		return nil, err
	}
	if !p.approvalPolicy {
		mode, err := a.ApprovalMode(ctx, sid, nil)
		if err != nil {
			return nil, err
		}
		reviewer := "auto_review"
		if mode == "manual" {
			reviewer = "user"
		}
		if mode == "yolo" {
			reviewer = "bypass"
		}
		return &ApprovalPolicy{Policy: "on_request", Reviewer: reviewer, Mode: mode, Automatic: mode != "manual", Legacy: true}, nil
	}
	if err := a.Load(ctx, sid); err != nil {
		return nil, err
	}
	raw, err := p.conn.CallExtension(ctx, "_otto/approval_policy/get", map[string]any{"sessionId": sid})
	if err != nil {
		return nil, p.wrap(err)
	}
	return decodeApprovalPolicy(raw)
}

func (a *Agent) SetApprovalPolicy(ctx context.Context, sid string, expectedRevision uint64, policy, reviewer string) (*ApprovalPolicy, error) {
	if expectedRevision == 0 || (policy != "on_request" && policy != "never") || (reviewer != "user" && reviewer != "auto_review" && reviewer != "bypass") {
		return nil, errors.New("invalid approval policy settings")
	}
	p, err := a.process(ctx)
	if err != nil {
		return nil, err
	}
	if !p.approvalPolicy {
		return nil, ErrApprovalPolicyUnsupported
	}
	if err := a.Load(ctx, sid); err != nil {
		return nil, err
	}
	raw, err := p.conn.CallExtension(ctx, "_otto/approval_policy/set", map[string]any{"sessionId": sid, "expectedRevision": expectedRevision, "policy": policy, "reviewer": reviewer})
	if err != nil {
		return nil, p.wrap(err)
	}
	return decodeApprovalPolicy(raw)
}

func decodeApprovalPolicy(raw json.RawMessage) (*ApprovalPolicy, error) {
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(raw, &fields); err != nil {
		return nil, err
	}
	for _, key := range []string{"policy", "reviewer", "revision", "mode", "automatic", "reviewer_active"} {
		if len(fields[key]) == 0 || string(fields[key]) == "null" {
			return nil, errors.New("incomplete approval policy response")
		}
	}
	var policy ApprovalPolicy
	if err := json.Unmarshal(raw, &policy); err != nil {
		return nil, err
	}
	if policy.Revision == 0 || (policy.Policy != "on_request" && policy.Policy != "never") || (policy.Reviewer != "user" && policy.Reviewer != "auto_review" && policy.Reviewer != "bypass") || (policy.Mode != "manual" && policy.Mode != "auto" && policy.Mode != "yolo") {
		return nil, errors.New("invalid approval policy response")
	}
	wantMode := "auto"
	if policy.Policy != "never" {
		if policy.Reviewer == "user" {
			wantMode = "manual"
		}
		if policy.Reviewer == "bypass" {
			wantMode = "yolo"
		}
	}
	if policy.Mode != wantMode || policy.Automatic != (policy.Policy == "never" || policy.Reviewer != "user") || policy.ReviewerActive != (policy.Policy == "on_request") {
		return nil, errors.New("inconsistent approval policy response")
	}
	return &policy, nil
}

func advertisesApprovalPolicy(meta map[string]any) bool {
	otto, _ := meta["otto"].(map[string]any)
	policy, _ := otto["approvalPolicy"].(map[string]any)
	return policy["version"] == float64(1) || policy["version"] == 1
}
