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
	return a.ApprovalMessageWithChannel(ctx, id, text, "", text)
}

// ApprovalMessageWithChannel supplies trusted channel metadata to the
// restricted approval dialogue, with a text fallback for older agents.
func (a *Agent) ApprovalMessageWithChannel(ctx context.Context, id, text, channel, legacyText string) (*ApprovalReply, error) {
	p, err := a.process(ctx)
	if err != nil {
		return nil, err
	}
	if !p.approvalDialogue {
		return nil, ErrApprovalUnsupported
	}
	params := map[string]any{"sessionId": id, "text": text}
	if p.channelContext {
		if channel == "telegram" || channel == "feishu" {
			params["channel"] = channel
		}
	} else {
		params["text"] = legacyText
	}
	raw, err := p.conn.CallExtension(ctx, "_otto/approvals/message", params)
	if err != nil {
		return nil, p.wrap(err)
	}
	var reply *ApprovalReply
	err = json.Unmarshal(raw, &reply)
	return reply, err
}
