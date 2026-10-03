package telegram

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"reflect"
	"strings"
	"sync"
	"testing"
	"time"
	"unicode/utf8"

	"github.com/baiyuqing/otto/connect/internal/bridge"
	"github.com/baiyuqing/otto/connect/internal/state"
)

const testToken = "123456:TEST-token-VALUE"

type resp struct {
	status int
	body   any
}

func okR(result any) resp { return resp{200, map[string]any{"ok": true, "result": result}} }

func errR(status int, desc string, retryAfter int) resp {
	return resp{status, map[string]any{
		"ok": false, "error_code": status, "description": desc,
		"parameters": map[string]any{"retry_after": retryAfter},
	}}
}

type call struct {
	method string
	body   map[string]any
}

// fakeAPI is an httptest Bot API server that records every call.
type fakeAPI struct {
	srv    *httptest.Server
	handle func(method string, body map[string]any) resp
	mu     sync.Mutex
	calls  []call
}

func newAPI(t *testing.T, handle func(method string, body map[string]any) resp) *fakeAPI {
	t.Helper()
	f := &fakeAPI{handle: handle}
	f.srv = httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		prefix := "/bot" + testToken + "/"
		if !strings.HasPrefix(r.URL.Path, prefix) || r.Method != http.MethodPost {
			t.Errorf("unexpected request %s %s", r.Method, r.URL.Path)
		}
		method := strings.TrimPrefix(r.URL.Path, prefix)
		var body map[string]any
		_ = json.NewDecoder(r.Body).Decode(&body)
		f.mu.Lock()
		f.calls = append(f.calls, call{method, body})
		f.mu.Unlock()
		res := f.handle(method, body)
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(res.status)
		_ = json.NewEncoder(w).Encode(res.body)
	}))
	t.Cleanup(f.srv.Close)
	return f
}

// botAPI answers getMe (id 99, "OttoBot"), serves the given getUpdates
// responses in order and then empty results, and accepts sends.
func botAPI(t *testing.T, polls ...resp) *fakeAPI {
	var mu sync.Mutex
	return newAPI(t, func(method string, _ map[string]any) resp {
		switch method {
		case "getMe":
			return okR(map[string]any{"id": 99, "username": "OttoBot"})
		case "getUpdates":
			mu.Lock()
			defer mu.Unlock()
			if len(polls) > 0 {
				r := polls[0]
				polls = polls[1:]
				return r
			}
			time.Sleep(5 * time.Millisecond)
			return okR([]any{})
		}
		return okR(true)
	})
}

func (f *fakeAPI) byMethod(method string) []call {
	f.mu.Lock()
	defer f.mu.Unlock()
	var out []call
	for _, c := range f.calls {
		if c.method == method {
			out = append(out, c)
		}
	}
	return out
}

func (f *fakeAPI) waitCalls(t *testing.T, method string, n int) {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for len(f.byMethod(method)) < n {
		if time.Now().After(deadline) {
			t.Fatalf("timed out waiting for %d %s calls", n, method)
		}
		time.Sleep(time.Millisecond)
	}
}

func openStore(t *testing.T) (*state.Store, string) {
	t.Helper()
	p := filepath.Join(t.TempDir(), "state.json")
	s, err := state.Open(p)
	if err != nil {
		t.Fatal(err)
	}
	return s, p
}

func newBot(api *fakeAPI, store *state.Store) *Bot {
	b := New(testToken, store)
	b.APIBase = api.srv.URL
	b.PollTimeout = 0
	b.backoffMin, b.backoffMax = time.Millisecond, 4*time.Millisecond
	return b
}

type running struct {
	msgs   chan bridge.Message
	cancel context.CancelFunc
	done   chan error
}

func start(b *Bot) *running {
	ctx, cancel := context.WithCancel(context.Background())
	r := &running{msgs: make(chan bridge.Message, 100), cancel: cancel, done: make(chan error, 1)}
	go func() { r.done <- b.Run(ctx, func(m bridge.Message) { r.msgs <- m }) }()
	return r
}

// stop cancels Run, waits for it and returns its result and the messages
// delivered so far.
func (r *running) stop(t *testing.T) (error, []bridge.Message) {
	t.Helper()
	r.cancel()
	select {
	case err := <-r.done:
		var got []bridge.Message
		for len(r.msgs) > 0 {
			got = append(got, <-r.msgs)
		}
		return err, got
	case <-time.After(5 * time.Second):
		t.Fatal("Run did not return after cancel")
		return nil, nil
	}
}

func upd(id int, msg map[string]any) map[string]any {
	return map[string]any{"update_id": id, "message": msg}
}

func privMsg(id int, text string) map[string]any {
	return map[string]any{
		"message_id": id, "from": map[string]any{"id": 7}, "date": 1700000000,
		"chat": map[string]any{"id": 7, "type": "private"}, "text": text,
	}
}

func groupMsg(id int, text string) map[string]any {
	return map[string]any{
		"message_id": id, "from": map[string]any{"id": 7}, "date": 1700000000,
		"chat": map[string]any{"id": -100123, "type": "supergroup"}, "text": text,
	}
}

func TestOffsetPersistedAndResumed(t *testing.T) {
	store, path := openStore(t)
	api := botAPI(t, okR([]any{upd(10, privMsg(1, "a")), upd(11, privMsg(2, "b"))}))
	r := start(newBot(api, store))
	api.waitCalls(t, "getUpdates", 2)
	err, got := r.stop(t)
	if err != nil {
		t.Fatal(err)
	}
	if len(got) != 2 || got[0].Text != "a" || got[1].Text != "b" {
		t.Fatalf("delivered %+v", got)
	}
	reopened, err := state.Open(path)
	if err != nil {
		t.Fatal(err)
	}
	if off := reopened.TelegramOffset(); off != 12 {
		t.Fatalf("persisted offset = %d, want 12", off)
	}

	api2 := botAPI(t)
	r2 := start(newBot(api2, reopened))
	api2.waitCalls(t, "getUpdates", 1)
	r2.stop(t)
	first := api2.byMethod("getUpdates")[0].body
	if first["offset"] != float64(12) {
		t.Errorf("first getUpdates offset = %v, want 12", first["offset"])
	}
	if !reflect.DeepEqual(first["allowed_updates"], []any{"message", "callback_query"}) {
		t.Errorf("allowed_updates = %v", first["allowed_updates"])
	}
	if _, ok := first["timeout"]; !ok {
		t.Error("getUpdates has no timeout")
	}
}

func TestGroupFilterAndTextNormalization(t *testing.T) {
	reply := func(id int, from int) map[string]any {
		m := groupMsg(id, "yes")
		m["reply_to_message"] = map[string]any{"message_id": 1, "from": map[string]any{"id": from}}
		return m
	}
	noFrom := privMsg(14, "no sender")
	delete(noFrom, "from")
	updates := []any{
		upd(1, groupMsg(1, "plain group text")),              // dropped: no mention
		upd(2, groupMsg(2, "@ottobot  do it")),               // mention removed
		upd(3, groupMsg(3, "hey @OttoBot")),                  // case-insensitive, trailing
		upd(4, groupMsg(4, "a @ottobot b")),                  // middle mention, one space left
		upd(5, groupMsg(5, "x@ottobot hi")),                  // dropped: not a token start
		upd(6, groupMsg(6, "@ottobotty hi")),                 // dropped: longer name
		upd(7, reply(7, 99)),                                 // reply to the bot
		upd(8, reply(8, 5)),                                  // dropped: reply to someone else
		upd(9, groupMsg(9, "/stop@OttoBot")),                 // command to the bot
		upd(10, groupMsg(10, "/stop@otherbot")),              // dropped: other bot
		upd(11, groupMsg(11, "/stop")),                       // dropped: unaddressed command
		upd(12, privMsg(12, "plain  text")),                  // private: unchanged
		upd(13, privMsg(13, "/new@ottobot now")),             // private: command reduced
		upd(14, noFrom),                                      // dropped: no sender
		upd(15, map[string]any{"edited_message": "ignored"}), // no message
	}
	store, path := openStore(t)
	api := botAPI(t, okR(updates))
	r := start(newBot(api, store))
	api.waitCalls(t, "getUpdates", 2)
	err, got := r.stop(t)
	if err != nil {
		t.Fatal(err)
	}
	type idText struct {
		ID, Text string
		Group    bool
	}
	var have []idText
	for _, m := range got {
		have = append(have, idText{m.MessageID, m.Text, m.Group})
	}
	want := []idText{
		{"2", "do it", true}, {"3", "hey", true}, {"4", "a b", true}, {"7", "yes", true},
		{"9", "/stop", true}, {"12", "plain  text", false}, {"13", "/new now", false},
	}
	if !reflect.DeepEqual(have, want) {
		t.Errorf("delivered\n got %+v\nwant %+v", have, want)
	}
	reopened, _ := state.Open(path)
	if off := reopened.TelegramOffset(); off != 16 {
		t.Errorf("offset = %d, want 16 (filtered updates advance it)", off)
	}
	m := got[0]
	if m.Platform != "telegram" || m.ChatID != "-100123" || m.SenderID != "7" || m.Time.Unix() != 1700000000 {
		t.Errorf("normalized fields: %+v", m)
	}
}

func TestMediaAndServiceMessages(t *testing.T) {
	photo := privMsg(1, "")
	delete(photo, "text")
	photo["photo"] = []any{map[string]any{"file_id": "f"}}
	photo["caption"] = "look at this"

	sticker := privMsg(2, "")
	delete(sticker, "text")
	sticker["sticker"] = map[string]any{"file_id": "s"}

	service := privMsg(3, "")
	delete(service, "text")
	service["new_chat_members"] = []any{map[string]any{"id": 5}}

	store, path := openStore(t)
	api := botAPI(t, okR([]any{upd(20, photo), upd(21, sticker), upd(22, service)}))
	r := start(newBot(api, store))
	api.waitCalls(t, "getUpdates", 2)
	_, got := r.stop(t)
	if len(got) != 2 {
		t.Fatalf("delivered %d messages, want 2: %+v", len(got), got)
	}
	if got[0].Text != "look at this" || !got[0].Attachment {
		t.Errorf("captioned photo: %+v", got[0])
	}
	if got[1].Text != "" || !got[1].Attachment {
		t.Errorf("sticker: %+v", got[1])
	}
	reopened, _ := state.Open(path)
	if off := reopened.TelegramOffset(); off != 23 {
		t.Errorf("offset = %d, want 23 (service message advances it)", off)
	}
}

func TestRejectedTokenStopsRun(t *testing.T) {
	for _, code := range []int{401, 404} {
		api := newAPI(t, func(string, map[string]any) resp {
			return errR(code, "Unauthorized for bot"+testToken, 0)
		})
		store, _ := openStore(t)
		err := newBot(api, store).Run(context.Background(), func(bridge.Message) {})
		if err == nil {
			t.Fatalf("code %d: Run returned nil", code)
		}
		if strings.Contains(err.Error(), testToken) {
			t.Errorf("code %d: error contains token: %v", code, err)
		}
		if n := len(api.byMethod("getUpdates")); n != 0 {
			t.Errorf("code %d: getUpdates called %d times after rejected getMe", code, n)
		}
	}
}

func TestRetriesTransientFailures(t *testing.T) {
	var mu sync.Mutex
	meCalls := 0
	polls := []resp{
		errR(502, "bad gateway", 0),
		errR(409, "Conflict: terminated by other getUpdates request", 0),
		errR(429, "Too Many Requests", 0),
		{500, "not json"},
		okR([]any{upd(5, privMsg(1, "after errors"))}),
	}
	api := newAPI(t, func(method string, _ map[string]any) resp {
		mu.Lock()
		defer mu.Unlock()
		switch method {
		case "getMe":
			meCalls++
			if meCalls == 1 {
				return errR(500, "internal", 0)
			}
			return okR(map[string]any{"id": 99, "username": "OttoBot"})
		case "getUpdates":
			if len(polls) > 0 {
				r := polls[0]
				polls = polls[1:]
				return r
			}
			return okR([]any{})
		}
		return okR(true)
	})
	store, _ := openStore(t)
	r := start(newBot(api, store))
	select {
	case m := <-r.msgs:
		if m.Text != "after errors" {
			t.Errorf("text = %q", m.Text)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("message not delivered after transient errors")
	}
	if err, _ := r.stop(t); err != nil {
		t.Fatal(err)
	}
	if store.TelegramOffset() != 6 {
		t.Errorf("offset = %d, want 6", store.TelegramOffset())
	}
}

func TestRunStopsOnContextDuringLongPoll(t *testing.T) {
	released := make(chan struct{})
	api := newAPI(t, func(method string, _ map[string]any) resp {
		if method == "getMe" {
			return okR(map[string]any{"id": 99, "username": "OttoBot"})
		}
		if method != "getUpdates" {
			return okR(true)
		}
		<-released // hold the poll open until the test ends
		return okR([]any{})
	})
	defer close(released)
	store, _ := openStore(t)
	b := newBot(api, store)
	r := start(b)
	api.waitCalls(t, "getUpdates", 1)
	if err, _ := r.stop(t); err != nil { // stop waits up to 5 s
		t.Fatal(err)
	}
}

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
	if err := b.Send(context.Background(), "42", "777", text); err != nil {
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
		if err := newBot(api, nil).Send(ctx, "1", "", text); err != nil {
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
		if err := b.Send(context.Background(), "1", "2", text); err != nil {
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
	if err := b.Send(context.Background(), "1", "", "hi"); err != nil {
		t.Fatal(err)
	}
	if n := len(sends(api)); n != 2 {
		t.Errorf("%d attempts, want 2", n)
	}

	mu.Lock()
	limited = 100
	mu.Unlock()
	before := len(sends(api))
	err := b.Send(context.Background(), "1", "", "hi")
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
	err := newBot(api, nil).Send(context.Background(), "1", "", "hi")
	if err == nil || !strings.Contains(err.Error(), "chat not found") {
		t.Errorf("err = %v", err)
	}
}

func TestTyping(t *testing.T) {
	api := botAPI(t)
	if err := newBot(api, nil).Typing(context.Background(), "-100123"); err != nil {
		t.Fatal(err)
	}
	c := api.byMethod("sendChatAction")
	if len(c) != 1 || c[0].body["action"] != "typing" || c[0].body["chat_id"] != float64(-100123) {
		t.Errorf("calls = %+v", c)
	}
}

type roundTripFunc func(*http.Request) (*http.Response, error)

func (f roundTripFunc) RoundTrip(r *http.Request) (*http.Response, error) { return f(r) }

func TestTokenNeverInErrors(t *testing.T) {
	ctx := context.Background()
	assertClean := func(name string, err error) {
		t.Helper()
		if err == nil {
			t.Errorf("%s: expected an error", name)
			return
		}
		if strings.Contains(err.Error(), testToken) || strings.Contains(err.Error(), "TEST-token-VALUE") {
			t.Errorf("%s: error contains the token: %v", name, err)
		}
	}

	// Transport failure: the connection is refused. net/http wraps this in
	// a *url.Error whose text is the request URL with the token.
	closed := httptest.NewServer(http.NotFoundHandler())
	b := newBot(botAPI(t), nil)
	b.APIBase = closed.URL
	closed.Close()
	assertClean("send/refused", b.Send(ctx, "1", "", "x"))
	assertClean("typing/refused", b.Typing(ctx, "1"))

	// Transport failure whose own message contains the URL.
	b = newBot(botAPI(t), nil)
	b.client = &http.Client{Transport: roundTripFunc(func(r *http.Request) (*http.Response, error) {
		return nil, errors.New("boom at " + r.URL.String())
	})}
	assertClean("send/custom transport", b.Send(ctx, "1", "", "x"))

	// Bot API error whose description echoes the token, and a non-JSON 5xx.
	api := newAPI(t, func(string, map[string]any) resp { return errR(400, "bad bot"+testToken, 0) })
	assertClean("send/api error", newBot(api, nil).Send(ctx, "1", "", "x"))
	api = newAPI(t, func(string, map[string]any) resp { return resp{502, "<html>/bot" + testToken + "</html>"} })
	assertClean("send/502", newBot(api, nil).Send(ctx, "1", "", "x"))

	// Run's getMe failure path.
	api = newAPI(t, func(string, map[string]any) resp { return errR(401, "Unauthorized", 0) })
	assertClean("run/401", newBot(api, nil).Run(ctx, func(bridge.Message) {}))
}

func TestApprovalButtonsLifecycle(t *testing.T) {
	id := strings.Repeat("a", 32)
	api := newAPI(t, func(method string, body map[string]any) resp { return okR(map[string]any{"message_id": 42}) })
	b := New(testToken, nil)
	b.APIBase = api.srv.URL
	finish, err := b.SendApproval(context.Background(), "123", id, strings.Repeat("x", 4000))
	if err != nil {
		t.Fatal(err)
	}
	calls := api.byMethod("sendMessage")
	buttons := calls[0].body["reply_markup"].(map[string]any)["inline_keyboard"].([]any)[0].([]any)
	for i, action := range []string{"approve", "deny"} {
		button := buttons[i].(map[string]any)
		q := &callbackQuery{ID: "q1", From: &user{ID: 7}, Message: &message{}, Data: button["callback_data"].(string)}
		q.Message.Chat.ID = 123
		m, ok := normalizeCallback(q)
		want := "/deny"
		if action == "approve" {
			want = "/allow"
		}
		if !ok || m.ApprovalID != id || m.Text != want || m.SenderID != "7" || m.ChatID != "123" {
			t.Fatalf("callback = %+v, %v", m, ok)
		}
	}
	if err = finish(context.Background(), "Approved."); err != nil {
		t.Fatal(err)
	}
	edit := api.byMethod("editMessageText")[0].body
	if edit["message_id"] != float64(42) || !strings.HasSuffix(edit["text"].(string), "Approved.") || len(edit["reply_markup"].(map[string]any)["inline_keyboard"].([]any)) != 0 {
		t.Fatal(edit)
	}
	if _, ok := normalizeCallback(&callbackQuery{Data: "approve:" + id}); ok {
		t.Fatal("missing sender/message accepted")
	}
}

func TestPollingDeliversAndAcknowledgesApproval(t *testing.T) {
	id := strings.Repeat("b", 32)
	api := botAPI(t, okR([]any{map[string]any{"update_id": 1, "callback_query": map[string]any{"id": "query1", "from": map[string]any{"id": 7}, "message": map[string]any{"message_id": 42, "chat": map[string]any{"id": 123}}, "data": "approve:" + id}}}))
	store, _ := openStore(t)
	b := newBot(api, store)
	r := start(b)
	api.waitCalls(t, "answerCallbackQuery", 1)
	_, got := r.stop(t)
	if len(got) != 1 || got[0].ApprovalID != id || got[0].Text != "/allow" {
		t.Fatalf("messages = %+v", got)
	}
	if api.byMethod("answerCallbackQuery")[0].body["callback_query_id"] != "query1" {
		t.Fatal("wrong acknowledgement")
	}
}

func TestCommandMenuSynced(t *testing.T) {
	api := botAPI(t)
	store, _ := openStore(t)
	r := start(newBot(api, store))
	api.waitCalls(t, "getUpdates", 1)
	r.stop(t)

	var want []any
	for _, c := range bridge.Commands {
		want = append(want, map[string]any{"command": c.Name, "description": c.Description})
	}
	set := api.byMethod("setMyCommands")
	if len(set) != 1 || !reflect.DeepEqual(set[0].body, map[string]any{"commands": want}) {
		t.Fatalf("setMyCommands calls = %+v, want one with commands %v", set, want)
	}
	del := api.byMethod("deleteMyCommands")
	if len(del) != 2 ||
		!reflect.DeepEqual(del[0].body, map[string]any{"scope": map[string]any{"type": "all_private_chats"}}) ||
		!reflect.DeepEqual(del[1].body, map[string]any{"scope": map[string]any{"type": "all_group_chats"}}) {
		t.Fatalf("deleteMyCommands calls = %+v", del)
	}
}

func TestCommandMenuFailureDoesNotStopRun(t *testing.T) {
	var mu sync.Mutex
	polled := false
	api := newAPI(t, func(method string, _ map[string]any) resp {
		switch method {
		case "getMe":
			return okR(map[string]any{"id": 99, "username": "OttoBot"})
		case "setMyCommands", "deleteMyCommands":
			return errR(400, "Bad Request", 0)
		case "getUpdates":
			mu.Lock()
			defer mu.Unlock()
			if !polled {
				polled = true
				return okR([]any{upd(1, privMsg(1, "hello"))})
			}
			time.Sleep(5 * time.Millisecond)
		}
		return okR([]any{})
	})
	store, _ := openStore(t)
	r := start(newBot(api, store))
	api.waitCalls(t, "deleteMyCommands", 2)
	select {
	case m := <-r.msgs:
		if m.Text != "hello" {
			t.Fatalf("delivered %q", m.Text)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("update not delivered after command menu failure")
	}
	if err, _ := r.stop(t); err != nil {
		t.Fatal(err)
	}
	if n := len(api.byMethod("setMyCommands")); n != 1 {
		t.Errorf("setMyCommands called %d times, want 1 (no retry)", n)
	}
}
