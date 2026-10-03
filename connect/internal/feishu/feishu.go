// Package feishu is the Feishu/Lark platform adapter. It uses the
// channel-sdk-go WebSocket long connection and sends replies as Markdown.
package feishu

import (
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
	"strings"
	"time"

	channel "github.com/larksuite/channel-sdk-go"
	"github.com/larksuite/channel-sdk-go/types"
	lark "github.com/larksuite/oapi-sdk-go/v3"
	larkcore "github.com/larksuite/oapi-sdk-go/v3/core"

	"github.com/baiyuqing/otto/connect/internal/bridge"
)

// noChat is the group allowlist used when no chat is configured. The SDK
// admits every group when GroupAllowlist is empty; no chat id equals this
// value.
const noChat = "\x00no-chat"

// Options configures New.
type Options struct {
	AppID     string
	AppSecret string
	Domain    string // "feishu" or "lark"
	Chats     []string
	Senders   []string
}

// sdk is the part of channel.Channel the adapter calls.
type sdk interface {
	Stream(context.Context, *types.SendInput) (types.StreamController, error)
	OnCardAction(func(context.Context, *types.CardActionEvent) error)
	OnMessage(handler func(ctx context.Context, msg *types.NormalizedMessage) error)
	OnError(handler func(err error))
	OnReject(handler func(ctx context.Context, event *types.RejectEvent) error)
	Start(ctx context.Context) error
	Stop(ctx context.Context) error
	Send(ctx context.Context, input *types.SendInput) (*types.SendResult, error)
}

// Platform implements bridge.Platform for one Feishu or Lark app.
type Platform struct {
	ch sdk
}

var _ bridge.Platform = (*Platform)(nil)

// New creates the SDK channel. It does not connect; Run does.
func New(o Options) (*Platform, error) {
	base := lark.FeishuBaseUrl
	if o.Domain == "lark" {
		base = lark.LarkBaseUrl
	}
	ch, err := channel.New(o.AppID, o.AppSecret,
		channel.WithDomain(base),
		channel.WithLogger(slogLogger{}),
		channel.WithPolicyConfig(policy(o.Chats, o.Senders)),
		channel.WithSafetyConfig(safety()),
	)
	if err != nil {
		return nil, fmt.Errorf("feishu: %w", err)
	}
	return &Platform{ch: ch}, nil
}

func (p *Platform) Name() string { return "feishu" }

// policy is the SDK admission policy for the configured lists. It is a
// second layer; the bridge checks the same lists. An empty list must not
// produce an open policy: the SDK treats an empty DMAllowlist with
// DMMode "allowlist" as closed, but an empty GroupAllowlist as open.
func policy(chats, senders []string) types.PolicyConfig {
	require := true
	pc := types.PolicyConfig{
		RequireMention: &require,
		DMMode:         "allowlist",
		DMAllowlist:    senders,
		GroupAllowlist: chats,
	}
	if len(senders) == 0 {
		pc.DMMode = "disabled"
	}
	if len(chats) == 0 {
		pc.GroupAllowlist = []string{noChat}
	}
	return pc
}

// staleWindow is the SDK stale window. The SDK drops older messages before
// the policy check and before OnMessage, so the bridge would never see them;
// the window is larger than the bridge's 30 minute rule so that the bridge
// drops stale messages and tells the chat.
const staleWindow = 24 * time.Hour

// safety is the SDK default safety configuration (event deduplication) with
// batching off and staleWindow. The SDK merges messages of one chat that
// arrive within 600 ms into one message that carries the last message's
// sender and id; that would attribute text of an unlisted sender to a listed
// one.
func safety() types.SafetyConfig {
	s := types.DefaultChannelConfig().Safety
	s.Batch.DelayMs = 0
	s.StaleMessageWindowMs = staleWindow
	return s
}

// Run connects and delivers messages until ctx ends. The SDK acknowledges an
// event when its dispatcher returns, which is before the handler runs, so a
// message is acknowledged once the SDK has queued it.
func (p *Platform) Run(ctx context.Context, deliver func(bridge.Message)) error {
	p.ch.OnCardAction(func(_ context.Context, e *types.CardActionEvent) error {
		if m, ok := cardMessage(e); ok {
			deliver(m)
		}
		return nil
	})
	p.ch.OnMessage(func(_ context.Context, m *types.NormalizedMessage) error {
		deliver(toMessage(m))
		return nil
	})
	// The SDK's own log line for these errors omits the error; it passes the
	// error only to OnError handlers (connection failures, reconnects).
	p.ch.OnError(func(err error) { slog.Warn("feishu connection error", "error", err) })
	p.ch.OnReject(func(_ context.Context, e *types.RejectEvent) error {
		logReject(e)
		return nil
	})
	errc := make(chan error, 1)
	// ponytail: Start blocks forever and ignores ctx; its goroutine ends
	// with the process.
	go func() { errc <- p.ch.Start(ctx) }()
	select {
	case <-ctx.Done():
		_ = p.ch.Stop(context.Background())
		return nil
	case err := <-errc:
		if ctx.Err() != nil {
			return nil
		}
		if err == nil {
			err = fmt.Errorf("connection ended")
		}
		return fmt.Errorf("feishu: %w", err)
	}
}

// logReject logs a message the SDK policy rejected with the bridge's
// rejection message and attribute keys, so one search finds both. The SDK
// drops these messages before OnMessage, so the bridge never sees them; the
// log line is where users find the chat and sender ids for the allowlists.
// The message text is not logged. A group message that does not mention the
// bot is not logged, as in the Telegram adapter: it is ordinary group
// traffic, and the SDK checks the group before the mention, so its chat is
// already allowed.
func logReject(e *types.RejectEvent) {
	if e.Reason == string(types.RejectReasonNoMention) {
		return
	}
	slog.Info("message rejected: chat or sender not allowed",
		"platform", "feishu", "chat", e.ChatID, "sender", e.SenderID, "reason", e.Reason)
}

// toMessage maps an SDK message. SenderID is the sender's open_id (the SDK
// falls back to user_id when the event has none).
func toMessage(m *types.NormalizedMessage) bridge.Message {
	t := time.UnixMilli(m.CreateTimeMs)
	if m.CreateTimeMs == 0 {
		t = time.Now()
	}
	out := bridge.Message{
		Platform:  "feishu",
		ChatID:    m.ChatID,
		SenderID:  m.UserID,
		MessageID: m.MessageID,
		Time:      t,
		Group:     m.ChatType == "group",
		Attachment: len(m.Resources) > 0 ||
			(m.RawContentType != "" && m.RawContentType != "text" && m.RawContentType != "post"),
	}
	// Other types normalize to placeholders such as "[image]"; they are not
	// text for the agent.
	if m.RawContentType == "" || m.RawContentType == "text" || m.RawContentType == "post" {
		out.Text = stripBotMention(m)
	}
	return out
}

// stripBotMention removes the bot's mention from the content: the key
// ("@_user_1") in a text message and "@name" in a rich-text one. Mentions of
// other users stay.
func stripBotMention(m *types.NormalizedMessage) string {
	text := m.Content
	for _, mn := range m.Mentions {
		if !mn.IsBot {
			continue
		}
		if mn.Key != "" {
			text = strings.ReplaceAll(text, mn.Key, "")
		}
		if mn.Name != "" {
			text = strings.ReplaceAll(text, "@"+mn.Name, "")
		}
	}
	return strings.TrimSpace(text)
}

// Send posts text as Markdown; the SDK splits it at 3500 runes, keeping code
// fences closed. A non-empty replyTo makes it a reply to that message.
func (p *Platform) Send(ctx context.Context, chatID, replyTo, text string, _ bool) error {
	_, err := p.ch.Send(ctx, &types.SendInput{ChatID: chatID, Markdown: text, ReplyMessageID: replyTo})
	if err != nil {
		return fmt.Errorf("feishu send: %w", err)
	}
	return nil
}

// Typing does nothing: Feishu has no typing indicator for bots.
func (p *Platform) Typing(context.Context, string) error { return nil }

// slogLogger routes SDK warnings and errors to slog. Debug and info are
// dropped: the SDK logs event payloads at debug and the WebSocket URL, which
// carries a connection ticket, at info.
type slogLogger struct{}

var _ larkcore.Logger = slogLogger{}

func (slogLogger) Debug(context.Context, ...interface{}) {}
func (slogLogger) Info(context.Context, ...interface{})  {}
func (slogLogger) Warn(_ context.Context, args ...interface{}) {
	slog.Warn("feishu sdk", "msg", format(args))
}
func (slogLogger) Error(_ context.Context, args ...interface{}) {
	msg := format(args)
	// The channel SDK has no read-receipt hook. This unused event is benign;
	// keep it visible at debug without hiding other dispatcher failures.
	if strings.HasPrefix(msg, "handle message failed,") &&
		strings.Contains(msg, "err: event type: im.message.message_read_v1, not found handler") {
		slog.Debug("feishu sdk", "msg", msg)
		return
	}
	slog.Error("feishu sdk", "msg", msg)
}

// format handles both Sprint-style and Printf-style SDK calls.
func format(args []interface{}) string {
	if len(args) == 0 {
		return ""
	}
	if f, ok := args[0].(string); ok && strings.Contains(f, "%") {
		return fmt.Sprintf(f, args[1:]...)
	}
	return strings.TrimSpace(fmt.Sprintln(args...))
}

func cardMessage(e *types.CardActionEvent) (bridge.Message, bool) {
	id, _ := e.Action.Value["request_id"].(string)
	action, _ := e.Action.Value["action"].(string)
	if len(id) != 32 || (action != "approve" && action != "deny") || e.ChatID == "" || e.Operator.OpenID == "" {
		return bridge.Message{}, false
	}
	text := "/deny"
	if action == "approve" {
		text = "/allow"
	}
	return bridge.Message{Platform: "feishu", ChatID: e.ChatID, SenderID: e.Operator.OpenID, ApprovalID: id, Text: text, Time: time.Now()}, true
}

func approvalCard(requestID, text, status string) string {
	elements := []any{map[string]any{"tag": "div", "text": map[string]any{"tag": "plain_text", "content": text}}}
	if status == "" {
		buttons := []any{}
		for _, b := range []struct{ label, action, style string }{{"Approve", "approve", "primary"}, {"Deny", "deny", "danger"}} {
			buttons = append(buttons, map[string]any{"tag": "button", "text": map[string]any{"tag": "plain_text", "content": b.label}, "type": b.style, "value": map[string]any{"request_id": requestID, "action": b.action}})
		}
		elements = append(elements, map[string]any{"tag": "action", "actions": buttons})
	} else {
		elements = append(elements, map[string]any{"tag": "div", "text": map[string]any{"tag": "plain_text", "content": status}})
	}
	raw, _ := json.Marshal(map[string]any{"config": map[string]any{"wide_screen_mode": true, "update_multi": true}, "elements": elements})
	return string(raw)
}

func (p *Platform) SendApproval(ctx context.Context, chatID, requestID, text string) (func(context.Context, string) error, error) {
	stream, err := p.ch.Stream(ctx, &types.SendInput{ChatID: chatID, Card: approvalCard(requestID, text, "")})
	if err != nil {
		return nil, err
	}
	return func(ctx context.Context, status string) error {
		defer stream.Close(ctx)
		return stream.UpdateCard(ctx, approvalCard(requestID, text, status))
	}, nil
}
