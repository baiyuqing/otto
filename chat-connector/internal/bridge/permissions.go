package bridge

import (
	"context"
	"fmt"
	"github.com/baiyuqing/otto/chat-connector/internal/agent"
	"strings"
	"time"
)

type PolicyPlatform interface {
	SendApprovalPolicy(ctx context.Context, chatID, messageID, session string, policy agent.ApprovalPolicy) error
}

func policySummary(sid string, policy *agent.ApprovalPolicy) string {
	label := agent.ApprovalModeLabel(policy.Mode)
	if policy.Policy == "never" {
		label = "NEVER"
	}
	active := "inactive"
	if policy.ReviewerActive {
		active = "active"
	}
	text := fmt.Sprintf("Approval policy: %s\nPolicy: %s\nReviewer: %s (%s)\nSession: %s", label, policy.Policy, policy.Reviewer, active, sid)
	if policy.Legacy {
		text += "\nLegacy agent; advanced policy changes require an Otto upgrade."
	}
	return text
}

func (c *chat) cmdPermissions(m Message, arg string) {
	fields := strings.Fields(arg)
	if len(fields) == 0 {
		c.cmdAuto(m, "")
		return
	}
	if len(fields) == 1 && (fields[0] == "manual" || fields[0] == "auto") {
		if fields[0] == "auto" {
			c.cmdAuto(m, "on")
		} else {
			c.cmdAuto(m, "off")
		}
		return
	}
	if len(fields) != 2 ||
		(fields[0] != "policy" && fields[0] != "reviewer") ||
		(fields[0] == "policy" && fields[1] != "on_request" && fields[1] != "never") ||
		(fields[0] == "reviewer" && fields[1] != "user" && fields[1] != "auto_review") {
		c.notify(m.MessageID, "Usage: /permissions [manual|auto | policy on_request|never | reviewer user|auto_review]")
		return
	}
	go func() {
		ctx, stop := context.WithTimeout(c.b.workCtx, 30*time.Second)
		defer stop()
		sid := c.b.opts.Store.Session(c.key)
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
		policy, err := c.b.opts.Agent.ApprovalPolicy(ctx, sid)
		if err == nil {
			if policy.Legacy {
				err = agent.ErrApprovalPolicyUnsupported
			} else {
				nextPolicy, reviewer := policy.Policy, policy.Reviewer
				if fields[0] == "policy" {
					nextPolicy = fields[1]
				} else {
					reviewer = fields[1]
				}
				if c.b.opts.Store.Session(c.key) != sid {
					c.send(ctx, m.MessageID, "This policy change belongs to another session.")
					return
				}
				policy, err = c.b.opts.Agent.SetApprovalPolicy(ctx, sid, policy.Revision, nextPolicy, reviewer)
			}
		}
		if err != nil {
			c.send(ctx, m.MessageID, "Could not configure approval policy: "+err.Error())
			return
		}
		c.startApprovalWatch(sid)
		c.send(ctx, m.MessageID, policySummary(sid, policy)+c.approvalCounts(sid))
	}()
}
