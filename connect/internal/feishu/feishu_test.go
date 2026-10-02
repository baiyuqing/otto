package feishu

import (
	"bytes"
	"context"
	"errors"
	"log/slog"
	"reflect"
	"slices"
	"strings"
	"testing"
	"time"

	channel "github.com/larksuite/channel-sdk-go"
	"github.com/larksuite/channel-sdk-go/types"

	"github.com/baiyuqing/otto/connect/internal/bridge"
)

func TestPolicyMapsLists(t *testing.T) {
	p := policy([]string{"oc_test"}, []string{"ou_test"})
	if p.DMMode != "allowlist" || !slices.Equal(p.DMAllowlist, []string{"ou_test"}) ||
		!slices.Equal(p.GroupAllowlist, []string{"oc_test"}) || p.RequireMention == nil || !*p.RequireMention {
		t.Errorf("policy = %+v", p)
	}
}

// The SDK admits every group when GroupAllowlist is empty and every DM when
// DMMode is "open" or unset, so empty lists must map to closed settings.
func TestPolicyEmptyListsAreClosed(t *testing.T) {
	p := policy(nil, nil)
	if p.DMMode != "disabled" {
		t.Errorf("DMMode = %q, want disabled", p.DMMode)
	}
	if len(p.GroupAllowlist) == 0 || slices.Contains(p.GroupAllowlist, "") {
		t.Errorf("GroupAllowlist = %q admits every group", p.GroupAllowlist)
	}
	if p.RequireMention == nil || !*p.RequireMention {
		t.Error("RequireMention must be true")
	}
}

// New must hand the SDK the mapped policy and a safety config without
// batching and with a stale window above the bridge's 30 minutes; it makes no network call.
func TestNewConfiguresSDK(t *testing.T) {
	p, err := New(Options{AppID: "cli_test", AppSecret: "secret", Domain: "lark", Chats: []string{"oc_test"}})
	if err != nil {
		t.Fatal(err)
	}
	got := p.ch.(channel.Channel).GetPolicy()
	if !reflect.DeepEqual(got, policy([]string{"oc_test"}, nil)) {
		t.Errorf("sdk policy = %+v", got)
	}
	if s := safety(); s.Batch.DelayMs != 0 || s.StaleMessageWindowMs <= 30*time.Minute {
		t.Errorf("safety = %+v", s)
	}
}

func TestNewRejectsEmptyCredentials(t *testing.T) {
	if _, err := New(Options{AppID: "cli_test"}); err == nil {
		t.Error("empty app secret accepted")
	}
}

func TestToMessage(t *testing.T) {
	ms := time.Date(2026, 10, 2, 8, 0, 0, 0, time.UTC).UnixMilli()
	bot := types.Mention{Key: "@_user_1", Name: "otto", OpenID: "ou_bot", IsBot: true}
	other := types.Mention{Key: "@_user_2", Name: "alice", OpenID: "ou_alice"}
	cases := []struct {
		name string
		in   types.NormalizedMessage
		want bridge.Message
	}{
		{"direct text", types.NormalizedMessage{
			MessageID: "om_1", ChatID: "oc_dm", ChatType: "p2p", UserID: "ou_test",
			Content: "hello", RawContentType: "text", CreateTimeMs: ms,
		}, bridge.Message{Platform: "feishu", ChatID: "oc_dm", SenderID: "ou_test", MessageID: "om_1", Text: "hello"}},
		{"group text strips only the bot", types.NormalizedMessage{
			MessageID: "om_2", ChatID: "oc_g", ChatType: "group", UserID: "ou_test",
			Content: "@_user_1 ask @_user_2 now", RawContentType: "text",
			Mentions: []types.Mention{bot, other}, MentionedBot: true, CreateTimeMs: ms,
		}, bridge.Message{Platform: "feishu", ChatID: "oc_g", SenderID: "ou_test", MessageID: "om_2", Group: true, Text: "ask @_user_2 now"}},
		{"post with text only is text", types.NormalizedMessage{
			MessageID: "om_3", ChatID: "oc_g", ChatType: "group", UserID: "ou_test",
			Content: "@otto **bold** line", RawContentType: "post",
			Mentions: []types.Mention{bot}, CreateTimeMs: ms,
		}, bridge.Message{Platform: "feishu", ChatID: "oc_g", SenderID: "ou_test", MessageID: "om_3", Group: true, Text: "**bold** line"}},
		{"image", types.NormalizedMessage{
			MessageID: "om_4", ChatID: "oc_dm", ChatType: "p2p", UserID: "ou_test",
			Content: "![image](img_1)", RawContentType: "image", CreateTimeMs: ms,
			Resources: []types.Resource{{Type: "image", FileKey: "img_1"}},
		}, bridge.Message{Platform: "feishu", ChatID: "oc_dm", SenderID: "ou_test", MessageID: "om_4", Attachment: true}},
		{"post with an image keeps its text", types.NormalizedMessage{
			MessageID: "om_5", ChatID: "oc_dm", ChatType: "p2p", UserID: "ou_test",
			Content: "see this", RawContentType: "post", CreateTimeMs: ms,
			Resources: []types.Resource{{Type: "image", FileKey: "img_1"}},
		}, bridge.Message{Platform: "feishu", ChatID: "oc_dm", SenderID: "ou_test", MessageID: "om_5", Text: "see this", Attachment: true}},
		{"sticker", types.NormalizedMessage{
			MessageID: "om_6", ChatID: "oc_dm", ChatType: "p2p", UserID: "ou_test",
			Content: "[sticker]", RawContentType: "sticker", CreateTimeMs: ms,
		}, bridge.Message{Platform: "feishu", ChatID: "oc_dm", SenderID: "ou_test", MessageID: "om_6", Attachment: true}},
		{"file", types.NormalizedMessage{
			MessageID: "om_7", ChatID: "oc_dm", ChatType: "p2p", UserID: "ou_test",
			Content: "[file]", RawContentType: "file", CreateTimeMs: ms,
			Resources: []types.Resource{{Type: "file", FileKey: "f_1", FileName: "a.txt"}},
		}, bridge.Message{Platform: "feishu", ChatID: "oc_dm", SenderID: "ou_test", MessageID: "om_7", Attachment: true}},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			c.want.Time = time.UnixMilli(ms)
			got := toMessage(&c.in)
			if !got.Time.Equal(c.want.Time) {
				t.Errorf("time = %v, want %v", got.Time, c.want.Time)
			}
			got.Time, c.want.Time = time.Time{}, time.Time{}
			if got != c.want {
				t.Errorf("got  %+v\nwant %+v", got, c.want)
			}
		})
	}
}

type fakeSDK struct {
	handler  func(context.Context, *types.NormalizedMessage) error
	onError  func(error)
	onReject func(context.Context, *types.RejectEvent) error
	startErr error
	started  chan struct{}
	stopped  chan struct{}
	sent     []types.SendInput
	sendErr  error
}

func newFake() *fakeSDK {
	return &fakeSDK{started: make(chan struct{}), stopped: make(chan struct{}, 1)}
}

func (f *fakeSDK) OnMessage(h func(context.Context, *types.NormalizedMessage) error) { f.handler = h }
func (f *fakeSDK) OnError(h func(error))                                             { f.onError = h }
func (f *fakeSDK) OnReject(h func(context.Context, *types.RejectEvent) error)        { f.onReject = h }
func (f *fakeSDK) Start(ctx context.Context) error {
	close(f.started)
	if f.startErr != nil {
		return f.startErr
	}
	select {} // like the SDK: blocks and ignores ctx
}
func (f *fakeSDK) Stop(context.Context) error { f.stopped <- struct{}{}; return nil }
func (f *fakeSDK) Send(_ context.Context, in *types.SendInput) (*types.SendResult, error) {
	f.sent = append(f.sent, *in)
	return &types.SendResult{}, f.sendErr
}

func TestSendArguments(t *testing.T) {
	f := newFake()
	p := &Platform{ch: f}
	if err := p.Send(context.Background(), "oc_test", "om_1", "**hi**"); err != nil {
		t.Fatal(err)
	}
	if err := p.Send(context.Background(), "oc_test", "", "plain"); err != nil {
		t.Fatal(err)
	}
	want := []types.SendInput{
		{ChatID: "oc_test", Markdown: "**hi**", ReplyMessageID: "om_1"},
		{ChatID: "oc_test", Markdown: "plain"},
	}
	if !reflect.DeepEqual(f.sent, want) {
		t.Errorf("sent = %+v\nwant %+v", f.sent, want)
	}
	f.sendErr = errors.New("boom")
	if err := p.Send(context.Background(), "oc_test", "", "x"); err == nil {
		t.Error("send error not returned")
	}
}

func TestTypingIsNoop(t *testing.T) {
	if err := (&Platform{ch: newFake()}).Typing(context.Background(), "oc_test"); err != nil {
		t.Error(err)
	}
}

func TestRunDeliversAndStopsOnCancel(t *testing.T) {
	f := newFake()
	p := &Platform{ch: f}
	ctx, cancel := context.WithCancel(context.Background())
	got := make(chan bridge.Message, 1)
	done := make(chan error, 1)
	go func() { done <- p.Run(ctx, func(m bridge.Message) { got <- m }) }()
	<-f.started
	// Handlers are registered before Start, so they are set here.
	if f.onReject == nil {
		t.Fatal("Run registered no OnReject handler")
	}
	var logs bytes.Buffer
	old := slog.Default()
	slog.SetDefault(slog.New(slog.NewTextHandler(&logs, nil)))
	defer slog.SetDefault(old)
	if err := f.onReject(context.Background(), &types.RejectEvent{MessageID: "om_9", ChatID: "oc_other", SenderID: "ou_other", Reason: "group_not_allowed"}); err != nil {
		t.Fatal(err)
	}
	for _, want := range []string{"message rejected: chat or sender not allowed", "platform=feishu", "chat=oc_other", "sender=ou_other", "reason=group_not_allowed"} {
		if !strings.Contains(logs.String(), want) {
			t.Errorf("reject log %q lacks %q", logs.String(), want)
		}
	}
	logs.Reset()
	if err := f.onReject(context.Background(), &types.RejectEvent{MessageID: "om_10", ChatID: "oc_test", SenderID: "ou_other", Reason: string(types.RejectReasonNoMention)}); err != nil {
		t.Fatal(err)
	}
	if logs.Len() != 0 {
		t.Errorf("a group message without a mention was logged: %q", logs.String())
	}
	if f.onError == nil {
		t.Fatal("Run registered no OnError handler")
	}
	err := f.handler(context.Background(), &types.NormalizedMessage{
		MessageID: "om_1", ChatID: "oc_test", ChatType: "p2p", UserID: "ou_test",
		Content: "hi", RawContentType: "text", CreateTimeMs: time.Now().UnixMilli(),
	})
	if err != nil {
		t.Fatalf("handler returned %v; a returned error makes the SDK report a failure", err)
	}
	if m := <-got; m.Text != "hi" || m.SenderID != "ou_test" || m.Platform != "feishu" {
		t.Errorf("delivered %+v", m)
	}
	cancel()
	if err := <-done; err != nil {
		t.Errorf("Run after cancel = %v, want nil", err)
	}
	select {
	case <-f.stopped:
	case <-time.After(time.Second):
		t.Error("Stop not called")
	}
}

func TestRunReturnsStartError(t *testing.T) {
	f := newFake()
	f.startErr = errors.New("invalid app credentials")
	err := (&Platform{ch: f}).Run(context.Background(), func(bridge.Message) {})
	if err == nil || err.Error() != "feishu: invalid app credentials" {
		t.Errorf("Run = %v", err)
	}
}

func TestSDKReadReceiptLogLevel(t *testing.T) {
	var logs bytes.Buffer
	old := slog.Default()
	slog.SetDefault(slog.New(slog.NewTextHandler(&logs, &slog.HandlerOptions{Level: slog.LevelDebug})))
	defer slog.SetDefault(old)
	for _, tc := range []struct{ event, detail, level string }{
		{"im.message.message_read_v1", "not found handler", "DEBUG"},
		{"im.message.receive_v1", "not found handler", "ERROR"},
		{"im.message.message_read_v1", "invalid payload", "ERROR"},
	} {
		logs.Reset()
		slogLogger{}.Error(context.Background(), "handle message failed, message_type: event, message_id: test, err: event type: %s, %s [conn_id=123]", tc.event, tc.detail)
		if !strings.Contains(logs.String(), "level="+tc.level) {
			t.Errorf("event %s (%s): got %s, want %s", tc.event, tc.detail, logs.String(), tc.level)
		}
	}
	logs.Reset()
	slog.SetDefault(slog.New(slog.NewTextHandler(&logs, nil)))
	slogLogger{}.Error(context.Background(), "handle message failed, err: event type: im.message.message_read_v1, not found handler [conn_id=123]")
	if logs.Len() != 0 {
		t.Fatalf("benign read receipt logged at default level: %s", logs.String())
	}
}
