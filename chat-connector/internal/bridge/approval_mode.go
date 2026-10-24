package bridge

import (
	"context"
	"fmt"
	"time"

	"github.com/baiyuqing/otto/chat-connector/internal/agent"
)

// ModePlatform presents session-bound shortcuts. The bridge still admits the
// sender and checks the binding when a shortcut is used. A non-empty message ID
// updates the clicked mode card instead of posting another one. `mode` is the
// exact session policy ("manual", "auto" or "yolo"); the shortcuts stay
// AUTO/MANUAL, so a chat participant cannot select YOLO from a button.
type ModePlatform interface {
	SendApprovalMode(ctx context.Context, chatID, messageID, session, mode string) error
}

// cmdAuto serves the hidden /auto alias and the /permissions manual|auto|no-argument forms.
func (c *chat) cmdAuto(m Message, arg string) {
	var automatic *bool
	switch arg {
	case "":
	case "on", "off":
		value := arg == "on"
		automatic = &value
	default:
		c.notify(m.MessageID, "Usage: /auto [on|off]")
		return
	}
	go func() {
		ctx, stop := context.WithTimeout(c.b.workCtx, 30*time.Second)
		defer stop()
		sid := c.b.opts.Store.Session(c.key)
		if m.ModeSession != "" && m.ModeSession != sid {
			c.send(ctx, m.MessageID, "This mode shortcut belongs to another session. Send /permissions for current controls.")
			return
		}
		var err error
		if sid == "" {
			sid, err = c.session(ctx, m)
		} else {
			err = c.b.load(ctx, sid)
		}
		if err != nil {
			c.send(ctx, m.MessageID, "Could not load the session: "+err.Error())
			return
		}
		c.startApprovalWatch(sid)
		if automatic != nil {
			if c.b.opts.Store.Session(c.key) != sid {
				c.send(ctx, m.MessageID, "This mode shortcut belongs to another session. Send /permissions for current controls.")
				return
			}
			_, err = c.b.opts.Agent.ApprovalMode(ctx, sid, automatic)
			if err != nil {
				c.send(ctx, m.MessageID, "Could not configure approval mode: "+err.Error())
				return
			}
		}
		policy, err := c.b.opts.Agent.ApprovalPolicy(ctx, sid)
		if err != nil {
			c.send(ctx, m.MessageID, "Could not configure approval mode: "+err.Error())
			return
		}
		mode := policy.Mode
		if policy.Policy == "never" {
			mode = "never"
		}
		messageID := ""
		if m.ModeSession != "" {
			messageID = m.MessageID
		}
		if platform, ok := c.p.(PolicyPlatform); ok && !policy.Legacy {
			if err := platform.SendApprovalPolicy(ctx, c.id, messageID, sid, *policy); err == nil {
				return
			}
		}
		if platform, ok := c.p.(ModePlatform); ok {
			messageID := ""
			if m.ModeSession != "" {
				messageID = m.MessageID
			}
			if err := platform.SendApprovalMode(ctx, c.id, messageID, sid, mode); err == nil {
				return
			}
		}
		name := agent.ApprovalModeLabel(mode)
		if !policy.Legacy {
			c.send(ctx, m.MessageID, policySummary(sid, policy)+c.approvalCounts(sid)+"\n/permissions auto · /permissions manual")
			return
		}
		note := "/permissions auto · /permissions manual"
		if mode == "yolo" {
			note = "This session approves every request without asking. /permissions manual requires manual approval again."
		}
		c.send(ctx, m.MessageID, fmt.Sprintf("Approval mode: %s\nSession: %s\n%s\nLegacy agent; advanced policy changes require an Otto upgrade.", name, sid, note))
	}()
}
