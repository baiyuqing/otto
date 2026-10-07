package agent

import (
	"context"
	"encoding/json"
	"strings"
)

// ApprovalMode uses Otto's session policy API, including during an active turn.
// It returns the session's mode name: "manual", "auto" or "yolo". `automatic`
// selects MANUAL or AUTO when it is not nil; YOLO is a mode name and has no
// boolean form, so a chat shortcut never selects it.
func (a *Agent) ApprovalMode(ctx context.Context, sid string, automatic *bool) (string, error) {
	p, err := a.process(ctx)
	if err != nil {
		return "", err
	}
	if err := a.Load(ctx, sid); err != nil {
		return "", err
	}
	method := "_otto/approval_mode/get"
	input := map[string]any{"sessionId": sid}
	if automatic != nil {
		method = "_otto/approval_mode/set"
		input["automatic"] = *automatic
	}
	raw, err := p.conn.CallExtension(ctx, method, input)
	if err != nil {
		return "", p.wrap(err)
	}
	var mode struct {
		Automatic bool   `json:"automatic"`
		Mode      string `json:"mode"`
	}
	if err := json.Unmarshal(raw, &mode); err != nil {
		return "", err
	}
	// An agent older than the mode field reports only the boolean.
	if mode.Mode == "" {
		if mode.Automatic {
			return "auto", nil
		}
		return "manual", nil
	}
	return mode.Mode, nil
}

// ApprovalModeLabel is the frontend name of a mode name.
func ApprovalModeLabel(mode string) string {
	return strings.ToUpper(mode)
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
