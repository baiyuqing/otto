package agent

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"

	"github.com/coder/acp-go-sdk"
)

var ErrTaskControlUnsupported = errors.New("agent does not support active-task control")
var ErrSteerRejected = errors.New("active task rejected guidance")

// ControlReply is the result of routing natural text during an active task.
// A nil reply means the task ended before it could be routed.
type ControlReply struct {
	Text   string `json:"text"`
	Queued bool   `json:"queued"`
}

// ControlMessageWithChannel asks the active task to interpret text as
// guidance or a follow-up. It never falls back to an ordinary prompt.
func (a *Agent) ControlMessageWithChannel(
	ctx context.Context,
	id, text, channel string,
) (*ControlReply, error) {
	p, err := a.process(ctx)
	if err != nil {
		return nil, err
	}
	if !p.taskControl {
		return nil, ErrTaskControlUnsupported
	}
	if !p.isOpen(id) {
		return nil, ErrNotOpen
	}
	params := map[string]any{"sessionId": id, "text": text}
	if channel == "telegram" || channel == "feishu" {
		params["channel"] = channel
	}
	raw, err := p.conn.CallExtension(ctx, "_otto/tasks/control", params)
	if err != nil {
		if isMethodNotFound(err) {
			return nil, ErrTaskControlUnsupported
		}
		return nil, p.wrap(err)
	}
	var reply *ControlReply
	if err := json.Unmarshal(raw, &reply); err != nil {
		return nil, fmt.Errorf("decode task control reply: %w", err)
	}
	return reply, nil
}

// Steer delivers explicit guidance to the active task without model routing.
func (a *Agent) Steer(ctx context.Context, id, text string) error {
	p, err := a.process(ctx)
	if err != nil {
		return err
	}
	if !p.taskControl {
		return ErrTaskControlUnsupported
	}
	if !p.isOpen(id) {
		return ErrNotOpen
	}
	raw, err := p.conn.CallExtension(ctx, "_otto/tasks/steer", map[string]string{
		"sessionId": id,
		"text":      text,
	})
	if err != nil {
		if isMethodNotFound(err) {
			return ErrTaskControlUnsupported
		}
		return p.wrap(err)
	}
	var reply struct {
		Accepted bool `json:"accepted"`
	}
	if err := json.Unmarshal(raw, &reply); err != nil {
		return fmt.Errorf("decode task steering reply: %w", err)
	}
	if !reply.Accepted {
		return ErrSteerRejected
	}
	return nil
}

func isMethodNotFound(err error) bool {
	var requestErr *acp.RequestError
	return errors.As(err, &requestErr) && requestErr.Code == -32601
}
