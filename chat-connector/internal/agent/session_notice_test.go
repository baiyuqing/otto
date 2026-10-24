package agent

import (
	"context"
	"math"
	"testing"

	"github.com/coder/acp-go-sdk"
)

func noticeChunk(fields map[string]any) acp.SessionUpdate {
	return wakeChunk(map[string]any{"otto": fields}, acp.TextBlock("memory proposals are pending"))
}

func TestSessionNoticeShape(t *testing.T) {
	valid := func() map[string]any {
		return map[string]any{"sessionNotice": true, "noticeId": float64(1), "presentationOrigin": "chat:telegram:c1"}
	}
	for _, origin := range []string{"chat:telegram:c1", ""} {
		fields := valid()
		fields["presentationOrigin"] = origin
		text, gotOrigin, ok := SessionNotice(noticeChunk(fields))
		if !ok || text != "memory proposals are pending" || gotOrigin != origin {
			t.Fatalf("notice = %q, %q, %v", text, gotOrigin, ok)
		}
	}
	for name, mutate := range map[string]func(map[string]any){
		"missing flag":          func(m map[string]any) { delete(m, "sessionNotice") },
		"false flag":            func(m map[string]any) { m["sessionNotice"] = false },
		"string flag":           func(m map[string]any) { m["sessionNotice"] = "true" },
		"missing id":            func(m map[string]any) { delete(m, "noticeId") },
		"string id":             func(m map[string]any) { m["noticeId"] = "1" },
		"zero id":               func(m map[string]any) { m["noticeId"] = float64(0) },
		"negative id":           func(m map[string]any) { m["noticeId"] = float64(-1) },
		"fractional id":         func(m map[string]any) { m["noticeId"] = 1.5 },
		"infinite id":           func(m map[string]any) { m["noticeId"] = math.Inf(1) },
		"nan id":                func(m map[string]any) { m["noticeId"] = math.NaN() },
		"missing origin":        func(m map[string]any) { delete(m, "presentationOrigin") },
		"non-string origin":     func(m map[string]any) { m["presentationOrigin"] = true },
		"old reflection marker": func(m map[string]any) { delete(m, "sessionNotice"); m["reflectionNotice"] = true },
	} {
		t.Run(name, func(t *testing.T) {
			fields := valid()
			mutate(fields)
			if _, _, ok := SessionNotice(noticeChunk(fields)); ok {
				t.Fatal("invalid notice accepted")
			}
		})
	}
	for name, update := range map[string]acp.SessionUpdate{
		"ordinary": acp.UpdateAgentMessageText("history"),
		"non-text": wakeChunk(map[string]any{"otto": valid()}, acp.ImageBlock("", "image/png")),
	} {
		if _, _, ok := SessionNotice(update); ok {
			t.Errorf("%s accepted", name)
		}
	}
}

func TestSessionUpdateDeliversSessionNoticeDuringLoad(t *testing.T) {
	rec := &recorder{}
	c := &client{p: &process{loading: map[string]bool{"s1": true}}, handler: rec}
	send := func(update acp.SessionUpdate) {
		t.Helper()
		if err := c.SessionUpdate(context.Background(), acp.SessionNotification{SessionId: "s1", Update: update}); err != nil {
			t.Fatal(err)
		}
	}
	send(acp.UpdateAgentMessageText("history"))
	send(noticeChunk(map[string]any{"reflectionNotice": true, "noticeId": float64(1), "presentationOrigin": "chat:fake:c1"}))
	send(noticeChunk(map[string]any{"sessionNotice": true, "noticeId": float64(0), "presentationOrigin": "chat:fake:c1"}))
	if rec.count() != 0 {
		t.Fatal("invalid notice or ordinary history replay delivered")
	}
	send(noticeChunk(map[string]any{"sessionNotice": true, "noticeId": float64(1), "presentationOrigin": "chat:fake:c1"}))
	if rec.count() != 1 {
		t.Fatalf("delivered %d updates, want the valid session notice", rec.count())
	}
}
