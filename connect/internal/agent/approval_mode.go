package agent

import (
	"context"
	"encoding/json"
)

// ApprovalMode uses Otto's session policy API, including during an active turn.
func (a *Agent) ApprovalMode(ctx context.Context, sid string, automatic *bool) (bool, error) {
	p, err := a.process(ctx)
	if err != nil {
		return false, err
	}
	if err := a.Load(ctx, sid); err != nil {
		return false, err
	}
	method := "_otto/approval_mode/get"
	input := map[string]any{"sessionId": sid}
	if automatic != nil {
		method = "_otto/approval_mode/set"
		input["automatic"] = *automatic
	}
	raw, err := p.conn.CallExtension(ctx, method, input)
	if err != nil {
		return false, p.wrap(err)
	}
	var mode struct {
		Automatic bool `json:"automatic"`
	}
	err = json.Unmarshal(raw, &mode)
	return mode.Automatic, err
}

type ApprovalChange struct {
	Sequence int64          `json:"sequence"`
	Request  SharedApproval `json:"request"`
}

func (a *Agent) ApprovalChanges(ctx context.Context, sid string, after int64) ([]ApprovalChange, error) {
	p, err := a.process(ctx)
	if err != nil {
		return nil, err
	}
	if !p.sharedApprovals {
		return nil, ErrSharedApprovalUnsupported
	}
	raw, err := p.conn.CallExtension(ctx, "_otto/approvals/watch", map[string]any{"sessionId": sid, "after": after})
	if err != nil {
		return nil, p.wrap(err)
	}
	var changes []ApprovalChange
	err = json.Unmarshal(raw, &changes)
	return changes, err
}
