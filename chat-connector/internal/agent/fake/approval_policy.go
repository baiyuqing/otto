package fake

import (
	"encoding/json"
	"errors"
	"github.com/coder/acp-go-sdk"
	"os"
)

type approvalPolicy struct {
	Policy         string `json:"policy"`
	Reviewer       string `json:"reviewer"`
	Revision       uint64 `json:"revision"`
	Mode           string `json:"mode"`
	Automatic      bool   `json:"automatic"`
	ReviewerActive bool   `json:"reviewer_active"`
}

func (a *agent) policyMethod(method string, params json.RawMessage) (any, error) {
	a.policyMu.Lock()
	defer a.policyMu.Unlock()
	var input struct {
		SessionID        string `json:"sessionId"`
		ExpectedRevision uint64 `json:"expectedRevision"`
		Policy           string `json:"policy"`
		Reviewer         string `json:"reviewer"`
		Automatic        *bool  `json:"automatic"`
	}
	if err := json.Unmarshal(params, &input); err != nil || input.SessionID == "" {
		return nil, errors.New("invalid session policy")
	}
	path := a.dir + "/policy-" + input.SessionID
	policy := approvalPolicy{Policy: "on_request", Reviewer: "auto_review", Revision: 1}
	if raw, err := os.ReadFile(path); err == nil {
		if err := json.Unmarshal(raw, &policy); err != nil {
			return nil, err
		}
	}
	changed := false
	if method == "_otto/approval_policy/set" {
		if input.ExpectedRevision != policy.Revision {
			return nil, &acp.RequestError{Code: -32001, Message: "stale approval settings revision"}
		}
		if (input.Policy != "on_request" && input.Policy != "never") || (input.Reviewer != "user" && input.Reviewer != "auto_review" && input.Reviewer != "bypass") {
			return nil, errors.New("invalid policy")
		}
		policy.Policy, policy.Reviewer = input.Policy, input.Reviewer
		a.log("policy:%s:%s:%s", input.SessionID, policy.Policy, policy.Reviewer)
		changed = true
	}
	if method == "_otto/approval_mode/set" {
		if input.Automatic == nil {
			return nil, errors.New("automatic required")
		}
		policy.Policy, policy.Reviewer = "on_request", "user"
		if *input.Automatic {
			policy.Reviewer = "auto_review"
		}
		a.log("auto:%s:%t", input.SessionID, *input.Automatic)
		changed = true
	}
	if changed {
		policy.Revision++
	}
	policy.Mode = "auto"
	if policy.Policy != "never" {
		if policy.Reviewer == "user" {
			policy.Mode = "manual"
		}
		if policy.Reviewer == "bypass" {
			policy.Mode = "yolo"
		}
	}
	policy.Automatic = policy.Policy == "never" || policy.Reviewer != "user"
	policy.ReviewerActive = policy.Policy == "on_request"
	if changed {
		raw, err := json.Marshal(policy)
		if err != nil {
			return nil, err
		}
		if err := os.WriteFile(path, raw, 0600); err != nil {
			return nil, err
		}
	}
	return policy, nil
}
