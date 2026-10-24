package bridge

import (
	"slices"
	"testing"

	"github.com/baiyuqing/otto/chat-connector/internal/agent/fake"
	"github.com/coder/acp-go-sdk"
)

func sessionNotice(origin, text string) acp.SessionUpdate {
	return acp.SessionUpdate{AgentMessageChunk: &acp.SessionUpdateAgentMessageChunk{
		Meta:    map[string]any{"otto": map[string]any{"sessionNotice": true, "noticeId": float64(1), "presentationOrigin": origin}},
		Content: acp.TextBlock(text),
	}}
}

func TestSessionNoticeReachesOnlyItsOriginChatAsPlainText(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{platform: "telegram", chats: []string{"c1", "c2"}})
	h.say("hello")
	h.waitSent(1)
	sid := h.store.Session("telegram:c1")
	h.b.chat(h.plat, "c2")
	if err := h.store.SetSession("telegram:c2", sid); err != nil {
		t.Fatal(err)
	}
	callsBefore := slices.Clone(fake.Calls(h.dir))
	h.b.Update(sid, sessionNotice("chat:telegram:c1", "memory: **1 candidate** queued"))
	h.waitSent(2)
	settle()
	h.plat.mu.Lock()
	defer h.plat.mu.Unlock()
	if len(h.plat.sent) != 2 {
		t.Fatalf("sent = %+v", h.plat.sent)
	}
	notice := h.plat.sent[1]
	if notice.chat != "c1" || notice.markdown || notice.text != "memory: **1 candidate** queued" {
		t.Fatalf("notice = %+v", notice)
	}
	if calls := fake.Calls(h.dir); !slices.Equal(calls, callsBefore) {
		t.Fatalf("notice reached the task model: %q", calls)
	}
}

func TestSessionNoticeRoutingRejectsUnrelatedOriginsAndSessions(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	h.say("hello")
	h.waitSent(1)
	sid := h.store.Session("fake:c1")
	for _, origin := range []string{"", "fake:c1", "chat:telegram:c1", "chat:fake:c2"} {
		h.b.Update(sid, sessionNotice(origin, "must not be broadcast"))
	}
	h.b.Update("other-session", sessionNotice("chat:fake:c1", "wrong session"))
	old := sessionNotice("chat:fake:c1", "old wire")
	old.AgentMessageChunk.Meta = map[string]any{"otto": map[string]any{"reflectionNotice": true, "noticeId": float64(1), "presentationOrigin": "chat:fake:c1"}}
	h.b.Update(sid, old)
	settle()
	if got := h.plat.texts(); len(got) != 1 {
		t.Fatalf("unrelated notice leaked: %q", got)
	}
}

func TestSessionNoticeRespectsRevokedAccess(t *testing.T) {
	for _, revoked := range []string{"chat", "senders"} {
		t.Run(revoked, func(t *testing.T) {
			store := newStore(t)
			id := sid("aaaa1111")
			if err := store.SetSession("fake:revoked", id); err != nil {
				t.Fatal(err)
			}
			s := setup{store: store}
			if revoked == "senders" {
				s.chats = []string{"revoked"}
				s.senders = []string{}
			}
			h := newHarness(t, s)
			h.b.chat(h.plat, "revoked")
			h.b.Update(id, sessionNotice("chat:fake:revoked", "private memory notice"))
			settle()
			if got := h.plat.texts(); len(got) != 0 {
				t.Fatalf("notice reached revoked chat: %q", got)
			}
		})
	}
}

func TestSessionNoticeWaitsForTheCurrentChatReply(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{platform: "telegram"})
	h.say("block")
	h.waitCall("start:block")
	sid := h.store.Session("telegram:c1")
	h.b.Update(sid, sessionNotice("chat:telegram:c1", "memory proposals are pending"))
	settle()
	if got := h.plat.texts(); len(got) != 0 {
		t.Fatalf("notice preceded task reply: %q", got)
	}
	h.say("/stop")
	if got := h.waitSent(2); !slices.Equal(got, []string{"Stopped.", "memory proposals are pending"}) {
		t.Fatalf("sent = %q", got)
	}
}

func TestSessionNoticeIsDroppedAfterTheChatSwitchesSessions(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	h.say("block")
	h.waitCall("start:block")
	sid := h.store.Session("fake:c1")
	h.b.Update(sid, sessionNotice("chat:fake:c1", "old session notice"))
	h.say("/new")
	h.waitSent(1)
	h.say("/stop")
	h.waitSent(2)
	settle()
	if got := h.plat.texts(); slices.Contains(got, "old session notice") {
		t.Fatalf("old session notice leaked: %q", got)
	}
}
