package telegram

import (
	"context"
	"errors"
	"strings"
	"sync"
	"testing"
	"unicode/utf8"
)

// sends returns the sendMessage bodies recorded by api.
func sends(api *fakeAPI) []map[string]any {
	var out []map[string]any
	for _, c := range api.byMethod("sendMessage") {
		out = append(out, c.body)
	}
	return out
}

func TestSendSplitsAtLineBoundaries(t *testing.T) {
	api := botAPI(t)
	b := newBot(api, nil)
	line := strings.Repeat("a", 2000) + "\n"
	text := line + line + line // 3 lines of 2001 units
	if err := b.Send(context.Background(), "42", "777", text, false); err != nil {
		t.Fatal(err)
	}
	got := sends(api)
	if len(got) != 2 {
		t.Fatalf("%d sendMessage calls, want 2", len(got))
	}
	if got[0]["text"] != strings.Repeat("a", 2000)+"\n"+strings.Repeat("a", 2000) || got[1]["text"] != strings.Repeat("a", 2000) {
		t.Errorf("parts not split at line boundaries: %d and %d bytes", len(got[0]["text"].(string)), len(got[1]["text"].(string)))
	}
	if got[0]["chat_id"] != float64(42) {
		t.Errorf("chat_id = %v (%T)", got[0]["chat_id"], got[0]["chat_id"])
	}
	if _, ok := got[0]["parse_mode"]; ok {
		t.Error("parse_mode must not be set")
	}
	rp, _ := got[0]["reply_parameters"].(map[string]any)
	if rp["message_id"] != float64(777) || rp["allow_sending_without_reply"] != true {
		t.Errorf("first part reply_parameters = %v", got[0]["reply_parameters"])
	}
	if _, ok := got[1]["reply_parameters"]; ok {
		t.Error("second part must not carry reply_parameters")
	}
}

func TestSendCountsUTF16Units(t *testing.T) {
	ctx := context.Background()
	check := func(name, text string, wantParts int) {
		t.Helper()
		api := botAPI(t)
		if err := newBot(api, nil).Send(ctx, "1", "", text, false); err != nil {
			t.Fatal(err)
		}
		got := sends(api)
		if len(got) != wantParts {
			t.Fatalf("%s: %d parts, want %d", name, len(got), wantParts)
		}
		var joined string
		for _, g := range got {
			p := g["text"].(string)
			if !utf8.ValidString(p) || utf16Len(p) > 4096 {
				t.Errorf("%s: invalid part (valid utf8 %v, %d units)", name, utf8.ValidString(p), utf16Len(p))
			}
			if _, ok := g["reply_parameters"]; ok {
				t.Errorf("%s: reply_parameters with empty replyTo", name)
			}
			joined += p
		}
		if strings.ReplaceAll(joined, "\n", "") != strings.ReplaceAll(text, "\n", "") {
			t.Errorf("%s: content changed by splitting", name)
		}
	}
	emoji := "😀" // 2 UTF-16 units, 4 bytes
	// 3000 units per line: two lines exceed 4096 units (but only 3000 runes).
	check("emoji lines", strings.Repeat(emoji, 1500)+"\n"+strings.Repeat(emoji, 1500), 2)
	// 4098 units in one line: hard split, never inside a surrogate pair.
	check("emoji long line", strings.Repeat(emoji, 2049), 2)
	// 4096 ASCII runes fit; 4097 do not.
	check("exact limit", strings.Repeat("x", 4000), 1)
	check("over limit", strings.Repeat("x", 4097), 2)
	// 4096 runes of 2 units each would pass a rune-count check but not the limit.
	check("rune count is not unit count", strings.Repeat(emoji, 4096), 2)
}

func TestSendEmptyTextSendsNothing(t *testing.T) {
	api := botAPI(t)
	b := newBot(api, nil)
	for _, text := range []string{"", "  \n "} {
		if err := b.Send(context.Background(), "1", "2", text, false); err != nil {
			t.Fatal(err)
		}
	}
	if n := len(api.calls); n != 0 {
		t.Errorf("%d requests for empty text", n)
	}
}

func TestSendRetriesRateLimit(t *testing.T) {
	var mu sync.Mutex
	limited := 1
	api := newAPI(t, func(string, map[string]any) resp {
		mu.Lock()
		defer mu.Unlock()
		if limited > 0 {
			limited--
			return errR(429, "Too Many Requests: retry after 0", 0)
		}
		return okR(true)
	})
	b := newBot(api, nil)
	if err := b.Send(context.Background(), "1", "", "hi", false); err != nil {
		t.Fatal(err)
	}
	if n := len(sends(api)); n != 2 {
		t.Errorf("%d attempts, want 2", n)
	}

	mu.Lock()
	limited = 100
	mu.Unlock()
	before := len(sends(api))
	err := b.Send(context.Background(), "1", "", "hi", false)
	var ae *apiError
	if !errors.As(err, &ae) || ae.Code != 429 {
		t.Fatalf("err = %v, want a 429 apiError", err)
	}
	if n := len(sends(api)) - before; n != sendAttempts {
		t.Errorf("%d attempts, want %d", n, sendAttempts)
	}
}

func TestSendReturnsAPIError(t *testing.T) {
	api := newAPI(t, func(string, map[string]any) resp { return errR(400, "Bad Request: chat not found", 0) })
	err := newBot(api, nil).Send(context.Background(), "1", "", "hi", false)
	if err == nil || !strings.Contains(err.Error(), "chat not found") {
		t.Errorf("err = %v", err)
	}
}
