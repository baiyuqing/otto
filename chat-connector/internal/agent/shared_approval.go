package agent

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
)

var ErrSharedApprovalUnsupported = errors.New("agent does not support unified approvals")

type SharedApproval struct {
	Review             *ReviewMetadata `json:"review,omitempty"`
	ID                 string          `json:"id"`
	Revision           uint64          `json:"revision"`
	Kind               string          `json:"kind"`
	Title              string          `json:"title"`
	Reason             string          `json:"reason"`
	Persistent         bool            `json:"persistent"`
	Blocking           bool            `json:"blocking"`
	PresentationOrigin string          `json:"presentation_origin"`
	State              string          `json:"state"`
	Application        string          `json:"application"`
	ApplicationError   string          `json:"application_error"`
	Decision           *struct {
		Transport string `json:"transport"`
	} `json:"decision"`
	Subject struct {
		Source  string `json:"source"`
		Version string `json:"version"`
		Details string `json:"details"`
	} `json:"subject"`
}

type ReviewAssessment struct {
	Risk              string `json:"risk"`
	UserAuthorization string `json:"user_authorization"`
	Outcome           string `json:"outcome"`
	Reason            string `json:"reason"`
}

type ReviewMetadata struct {
	Status           string            `json:"status"`
	Assessment       *ReviewAssessment `json:"assessment"`
	SettingsRevision uint64            `json:"settings_revision"`
	EvidenceDigest   string            `json:"evidence_digest"`
	PolicyDigest     string            `json:"policy_digest"`
}

func (r SharedApproval) HumanPending() bool {
	return r.State == "pending" && (r.Review == nil || r.Review.Status == "human_required")
}

type ApprovalSnapshot struct {
	Requests []SharedApproval `json:"requests"`
	Cursor   int64            `json:"cursor"`
}

func (a *Agent) SupportsSharedApprovals(ctx context.Context) (bool, error) {
	p, err := a.process(ctx)
	if err != nil {
		return false, err
	}
	return p.sharedApprovals, nil
}

func (a *Agent) SharedApprovals(ctx context.Context, sid string) (*ApprovalSnapshot, error) {
	p, err := a.process(ctx)
	if err != nil {
		return nil, err
	}
	if !p.sharedApprovals {
		return nil, ErrSharedApprovalUnsupported
	}
	if err := a.Load(ctx, sid); err != nil {
		return nil, err
	}
	raw, err := p.conn.CallExtension(ctx, "_otto/approvals/list", map[string]string{"sessionId": sid})
	if err != nil {
		return nil, p.wrap(err)
	}
	var page ApprovalSnapshot
	err = json.Unmarshal(raw, &page)
	return &page, err
}
func (a *Agent) DecideSharedApproval(ctx context.Context, sid string, r SharedApproval, option, feedback string) error {
	p, err := a.process(ctx)
	if err != nil {
		return err
	}
	if !p.sharedApprovals {
		return ErrSharedApprovalUnsupported
	}
	if err := a.Load(ctx, sid); err != nil {
		return err
	}
	raw, err := p.conn.CallExtension(ctx, "_otto/approvals/decide", map[string]any{"sessionId": sid, "requestId": r.ID, "revision": r.Revision, "optionId": option, "feedback": feedback})
	if err != nil {
		return err
	}
	var decided SharedApproval
	if err := json.Unmarshal(raw, &decided); err != nil {
		return err
	}
	if decided.Application == "failed" || decided.Application == "unknown" {
		return fmt.Errorf("Approval decision recorded as %s; application %s: %s", decided.State, decided.Application, decided.ApplicationError)
	}
	return nil
}
