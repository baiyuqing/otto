package agent

import (
	"context"
	"encoding/json"
	"errors"
)

var ErrApprovalUnsupported = errors.New("agent does not support approval dialogue")

// ApprovalReply is a restricted dialogue response; nil means no approval waits.
type ApprovalReply struct {
	Text   string `json:"text"`
	Queued bool   `json:"queued"`
}

func (a *Agent) ApprovalMessage(ctx context.Context, id, text string) (*ApprovalReply, error) {
	p, err := a.process(ctx)
	if err != nil {
		return nil, err
	}
	if !p.approvalDialogue {
		return nil, ErrApprovalUnsupported
	}
	raw, err := p.conn.CallExtension(ctx, "_otto/approvals/message", map[string]string{"sessionId": id, "text": text})
	if err != nil {
		return nil, p.wrap(err)
	}
	var reply *ApprovalReply
	err = json.Unmarshal(raw, &reply)
	return reply, err
}
