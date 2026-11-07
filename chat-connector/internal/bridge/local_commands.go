package bridge

import (
	"context"
	"encoding/json"
	"errors"
	"strings"
)

// localCommands renders independent human-operation receipts, never agent chunks.
func (b *Bridge) localCommands(_ context.Context, params json.RawMessage) (any, error) {
	var update struct {
		SessionID string `json:"sessionId"`
		Receipts  []struct {
			ID         string `json:"id"`
			Command    string `json:"command"`
			Status     string `json:"status"`
			Summary    string `json:"summary"`
			Parameters string `json:"parameters"`
			Output     string `json:"output"`
		} `json:"receipts"`
	}
	if err := json.Unmarshal(params, &update); err != nil || update.SessionID == "" {
		return nil, errors.New("invalid local command receipt update")
	}
	if len(update.Receipts) == 0 {
		return map[string]any{}, nil
	}
	receipt := update.Receipts[len(update.Receipts)-1]
	marker := "!"
	if receipt.Status == "success" {
		marker = "✓"
	} else if receipt.Status == "error" {
		marker = "✗"
	}
	text := "›_ LOCAL COMMAND " + marker + " " + receipt.Command + " — " + receipt.Summary
	if receipt.Parameters != "" {
		text += "\nParameters: " + receipt.Parameters
	}
	if strings.TrimSpace(receipt.Output) != "" {
		text += "\n" + receipt.Output
	}
	b.mu.Lock()
	chats := make([]*chat, 0, len(b.chats))
	for _, c := range b.chats {
		if b.opts.Store.Session(c.key) == update.SessionID {
			chats = append(chats, c)
		}
	}
	b.mu.Unlock()
	for _, c := range chats {
		c.sendBackground(update.SessionID, text, false)
	}
	return map[string]any{}, nil
}
