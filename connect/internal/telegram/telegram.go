// Package telegram is the Telegram Bot API platform adapter. It uses
// getUpdates long polling over net/http. Connector notices are plain text;
// the agent's Markdown replies are rendered to text and entities.
package telegram

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"net/url"
	"regexp"
	"strconv"
	"strings"
	"time"

	"github.com/baiyuqing/otto/connect/internal/bridge"
	"github.com/baiyuqing/otto/connect/internal/state"
)

const (
	// maxUnits is the sendMessage text limit in UTF-16 code units.
	maxUnits = 4096
	// sendAttempts bounds the retries of a rate-limited (429) sendMessage.
	sendAttempts = 3
)

// Bot implements bridge.Platform for one bot token.
type Bot struct {
	APIBase     string        // default https://api.telegram.org
	PollTimeout time.Duration // getUpdates long-poll timeout; default 50 s

	token  string
	store  *state.Store
	client *http.Client

	backoffMin, backoffMax time.Duration

	// Set by Run before the first update is handled; read only by Run.
	botID    int64
	mention  *regexp.Regexp // "@username" as a whole token, with trailing spaces
	cmdToBot *regexp.Regexp // leading "/cmd@username"
}

var _ bridge.Platform = (*Bot)(nil)

// New returns a Bot that persists its getUpdates offset in store.
func New(token string, store *state.Store) *Bot {
	return &Bot{
		APIBase:     "https://api.telegram.org",
		PollTimeout: 50 * time.Second,
		token:       token,
		store:       store,
		client:      &http.Client{},
		backoffMin:  time.Second,
		backoffMax:  30 * time.Second,
	}
}

func (b *Bot) Name() string { return "telegram" }

// apiError is a Bot API failure: an HTTP status of 400 or above, or ok:false.
type apiError struct {
	Method     string
	Status     int
	Code       int
	Desc       string
	RetryAfter int // seconds, from parameters.retry_after
}

func (e *apiError) Error() string {
	return fmt.Sprintf("telegram %s: status %d, error_code %d: %s", e.Method, e.Status, e.Code, e.Desc)
}

// transportError is a failure before a Bot API response was read. Its text
// is the unwrapped cause with the token removed; net/http errors otherwise
// contain the request URL, which contains the token.
type transportError struct {
	msg   string
	cause error
}

func (e *transportError) Error() string { return e.msg }
func (e *transportError) Unwrap() error { return e.cause }

type envelope struct {
	OK          bool            `json:"ok"`
	Result      json.RawMessage `json:"result"`
	Description string          `json:"description"`
	ErrorCode   int             `json:"error_code"`
	Parameters  struct {
		RetryAfter int `json:"retry_after"`
	} `json:"parameters"`
}

// call posts params as JSON to the method endpoint and decodes result into
// out (if non-nil). It never returns an error containing the token.
func (b *Bot) call(ctx context.Context, method string, params, out any) error {
	body, err := json.Marshal(params)
	if err != nil {
		return fmt.Errorf("telegram %s: %w", method, err)
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, b.APIBase+"/bot"+b.token+"/"+method, bytes.NewReader(body))
	if err != nil {
		return b.transportErr(method, err)
	}
	req.Header.Set("Content-Type", "application/json")
	resp, err := b.client.Do(req)
	if err != nil {
		return b.transportErr(method, err)
	}
	defer resp.Body.Close()
	raw, err := io.ReadAll(io.LimitReader(resp.Body, 16<<20))
	if err != nil {
		return b.transportErr(method, err)
	}
	var env envelope
	jsonErr := json.Unmarshal(raw, &env)
	if resp.StatusCode >= 400 || (jsonErr == nil && !env.OK) {
		return &apiError{
			Method: method, Status: resp.StatusCode, Code: env.ErrorCode,
			Desc:       strings.ReplaceAll(env.Description, b.token, "<token>"),
			RetryAfter: env.Parameters.RetryAfter,
		}
	}
	if jsonErr != nil {
		return fmt.Errorf("telegram %s: undecodable response (status %d)", method, resp.StatusCode)
	}
	if out != nil {
		if err := json.Unmarshal(env.Result, out); err != nil {
			return fmt.Errorf("telegram %s: undecodable result", method)
		}
	}
	return nil
}

func (b *Bot) transportErr(method string, err error) error {
	cause := err
	var ue *url.Error
	if errors.As(err, &ue) {
		cause = ue.Err
	}
	msg := strings.ReplaceAll(cause.Error(), b.token, "<token>")
	return &transportError{msg: "telegram " + method + ": " + msg, cause: cause}
}

// sleep waits d or until ctx ends; it reports whether the full wait elapsed.
func sleep(ctx context.Context, d time.Duration) bool {
	t := time.NewTimer(d)
	defer t.Stop()
	select {
	case <-t.C:
		return true
	case <-ctx.Done():
		return false
	}
}

// backoff waits before the next attempt after err: retry_after for a 429,
// otherwise *delay, which is then doubled up to backoffMax. It returns false
// when ctx ended.
func (b *Bot) backoff(ctx context.Context, err error, delay *time.Duration) bool {
	var ae *apiError
	if errors.As(err, &ae) && ae.Code == http.StatusTooManyRequests {
		return sleep(ctx, time.Duration(ae.RetryAfter)*time.Second)
	}
	d := *delay
	*delay = min(d*2, b.backoffMax)
	return sleep(ctx, d)
}

type user struct {
	ID       int64  `json:"id"`
	Username string `json:"username"`
}

type message struct {
	MessageID int64 `json:"message_id"`
	From      *user `json:"from"`
	Chat      struct {
		ID   int64  `json:"id"`
		Type string `json:"type"`
	} `json:"chat"`
	Date           int64    `json:"date"`
	Text           string   `json:"text"`
	Caption        string   `json:"caption"`
	ReplyToMessage *message `json:"reply_to_message"`

	Photo, Document, Video, Audio, Voice, VideoNote, Sticker, Animation json.RawMessage
}

func (m *message) hasAttachment() bool {
	for _, f := range []json.RawMessage{m.Photo, m.Document, m.Video, m.Audio, m.Voice, m.VideoNote, m.Sticker, m.Animation} {
		if len(f) > 0 && string(f) != "null" {
			return true
		}
	}
	return false
}

type update struct {
	UpdateID int64          `json:"update_id"`
	Message  *message       `json:"message"`
	Callback *callbackQuery `json:"callback_query"`
}

// syncCommands sets the default-scope command menu to bridge.Commands and
// deletes the private-chat and group-chat lists, which Telegram would show
// instead. Each call is tried once; a failure is logged and the rest still run.
func (b *Bot) syncCommands(ctx context.Context) {
	cmds := make([]map[string]string, len(bridge.Commands))
	for i, c := range bridge.Commands {
		cmds[i] = map[string]string{"command": c.Name, "description": c.Description}
	}
	calls := []struct {
		method string
		body   any
	}{
		{"setMyCommands", map[string]any{"commands": cmds}},
		{"deleteMyCommands", map[string]any{"scope": map[string]string{"type": "all_private_chats"}}},
		{"deleteMyCommands", map[string]any{"scope": map[string]string{"type": "all_group_chats"}}},
	}
	for _, c := range calls {
		if err := b.call(ctx, c.method, c.body, nil); err != nil {
			slog.Warn("telegram command menu update failed", "method", c.method, "err", err)
		}
	}
}

// Run identifies the bot with getMe, then polls getUpdates until ctx ends.
// It returns an error only when getMe is rejected with 401 or 404.
func (b *Bot) Run(ctx context.Context, deliver func(bridge.Message)) error {
	var me user
	delay := b.backoffMin
	for {
		err := b.call(ctx, "getMe", struct{}{}, &me)
		if err == nil {
			break
		}
		if ctx.Err() != nil {
			return nil
		}
		var ae *apiError
		if errors.As(err, &ae) && (ae.Code == 401 || ae.Code == 404) {
			return fmt.Errorf("telegram rejected the bot token: %w", err)
		}
		slog.Warn("telegram getMe failed", "err", err)
		if !b.backoff(ctx, err, &delay) {
			return nil
		}
	}
	b.botID = me.ID
	name := regexp.QuoteMeta(me.Username)
	b.mention = regexp.MustCompile(`(?i)(^|[^A-Za-z0-9_])@` + name + `\b\s*`)
	b.cmdToBot = regexp.MustCompile(`(?i)^(/\w+)@` + name + `\b`)
	slog.Info("telegram bot ready", "username", me.Username)
	b.syncCommands(ctx)

	// next is kept in memory so a failed persist does not re-fetch updates
	// that were already handled.
	next := b.store.TelegramOffset()
	delay = b.backoffMin
	for ctx.Err() == nil {
		var updates []update
		err := b.call(ctx, "getUpdates", map[string]any{
			"offset":          next,
			"timeout":         int(b.PollTimeout / time.Second),
			"allowed_updates": []string{"message", "callback_query"},
		}, &updates)
		if err != nil {
			if ctx.Err() != nil {
				return nil
			}
			var ae *apiError
			if errors.As(err, &ae) && ae.Code == http.StatusConflict {
				slog.Warn("telegram getUpdates conflict: another client is polling this bot token", "err", err)
			} else {
				slog.Warn("telegram getUpdates failed", "err", err)
			}
			if !b.backoff(ctx, err, &delay) {
				return nil
			}
			continue
		}
		delay = b.backoffMin
		for _, u := range updates {
			if u.Callback != nil {
				if m, ok := normalizeCallback(u.Callback); ok {
					deliver(m)
				}
				if err := b.call(ctx, "answerCallbackQuery", map[string]any{"callback_query_id": u.Callback.ID}, nil); err != nil {
					slog.Warn("telegram callback acknowledgement failed", "err", err)
				}
			}
			if m, ok := b.normalize(u.Message); ok {
				deliver(m)
			}
			next = u.UpdateID + 1
			if err := b.store.SetTelegramOffset(next); err != nil {
				slog.Error("telegram offset not persisted", "err", err)
			}
		}
	}
	return nil
}

// normalize converts a Bot API message to a bridge.Message. ok is false for
// updates without a message, without a sender, without content, and for group
// messages not addressed to the bot.
func (b *Bot) normalize(m *message) (bridge.Message, bool) {
	if m == nil || m.From == nil {
		return bridge.Message{}, false
	}
	text := m.Text
	if text == "" {
		text = m.Caption
	}
	attachment := m.hasAttachment()
	if text == "" && !attachment {
		return bridge.Message{}, false
	}
	group := m.Chat.Type == "group" || m.Chat.Type == "supergroup"

	addressed := b.cmdToBot.MatchString(text)
	text = b.cmdToBot.ReplaceAllString(text, "$1")
	mentioned := b.mention.MatchString(text)
	text = strings.TrimSpace(b.mention.ReplaceAllString(text, "$1"))
	replyToBot := m.ReplyToMessage != nil && m.ReplyToMessage.From != nil && m.ReplyToMessage.From.ID == b.botID
	if group && !mentioned && !addressed && !replyToBot {
		return bridge.Message{}, false
	}
	return bridge.Message{
		Platform:   "telegram",
		ChatID:     strconv.FormatInt(m.Chat.ID, 10),
		SenderID:   strconv.FormatInt(m.From.ID, 10),
		MessageID:  strconv.FormatInt(m.MessageID, 10),
		Text:       text,
		Time:       time.Unix(m.Date, 0),
		Group:      group,
		Attachment: attachment,
	}, true
}

// chatParam is the chat_id JSON value: a number for numeric ids, otherwise
// the string (for example "@channel").
func chatParam(chatID string) any {
	if n, err := strconv.ParseInt(chatID, 10, 64); err == nil {
		return n
	}
	return chatID
}

// Send posts text in parts of at most 4096 UTF-16 code units. Only the first
// part replies to replyTo. With markdown, text is the agent's Markdown reply:
// it is rendered to plain text and entities. A part that Telegram rejects with
// status 400 is resent once without entities.
func (b *Bot) Send(ctx context.Context, chatID, replyTo, text string, markdown bool) error {
	var ents []entity
	if markdown {
		text, ents = renderMarkdown(text)
	}
	for i, part := range splitSpans(text, maxUnits) {
		params := map[string]any{"chat_id": chatParam(chatID), "text": part.text}
		if pe := clipEntities(ents, part.start, utf16Len(part.text)); len(pe) > 0 {
			params["entities"] = pe
		}
		if id, err := strconv.ParseInt(replyTo, 10, 64); err == nil && i == 0 {
			params["reply_parameters"] = map[string]any{"message_id": id, "allow_sending_without_reply": true}
		}
		err := b.sendWithRetry(ctx, params)
		var ae *apiError
		if _, has := params["entities"]; has && errors.As(err, &ae) && ae.Status == http.StatusBadRequest {
			slog.Warn("telegram rejected entities; resending part as plain text", "err", err)
			delete(params, "entities")
			err = b.sendWithRetry(ctx, params)
		}
		if err != nil {
			return err
		}
	}
	return nil
}

// clipEntities returns the entities that overlap [start, start+length),
// clipped to it and rebased to start.
func clipEntities(ents []entity, start, length int) []entity {
	var out []entity
	for _, e := range ents {
		lo, hi := max(e.Offset, start), min(e.Offset+e.Length, start+length)
		if hi > lo {
			e.Offset, e.Length = lo-start, hi-lo
			out = append(out, e)
		}
	}
	return out
}

func (b *Bot) sendWithRetry(ctx context.Context, params map[string]any) error {
	for attempt := 1; ; attempt++ {
		err := b.call(ctx, "sendMessage", params, nil)
		var ae *apiError
		if err == nil || attempt == sendAttempts || !errors.As(err, &ae) || ae.Code != http.StatusTooManyRequests {
			return err
		}
		if !sleep(ctx, time.Duration(ae.RetryAfter)*time.Second) {
			return ctx.Err()
		}
	}
}

// Typing shows the typing indicator; Telegram clears it after 5 seconds.
func (b *Bot) Typing(ctx context.Context, chatID string) error {
	return b.call(ctx, "sendChatAction", map[string]any{"chat_id": chatParam(chatID), "action": "typing"}, nil)
}

func units(r rune) int {
	if r >= 0x10000 {
		return 2
	}
	return 1
}

func utf16Len(s string) int {
	n := 0
	for _, r := range s {
		n += units(r)
	}
	return n
}

// span is a part of split text and the UTF-16 offset of its start in the
// original text.
type span struct {
	text  string
	start int
}

// splitText splits text into non-blank parts of at most limit UTF-16 code
// units. It breaks after newlines where possible; a line longer than limit is
// cut between runes, so a surrogate pair is never divided. Trailing newlines
// are removed from each part.
func splitText(text string, limit int) []string {
	var parts []string
	for _, p := range splitSpans(text, limit) {
		parts = append(parts, p.text)
	}
	return parts
}

// splitSpans is splitText with each part's start offset. The only characters
// not in any part are the trailing newlines removed from a part and the text
// of blank parts, so offsets stay exact.
func splitSpans(text string, limit int) []span {
	var parts []span
	var cur strings.Builder
	curLen, pos, start := 0, 0, 0 // pos: units consumed; start: units before cur
	flush := func() {
		if p := strings.TrimRight(cur.String(), "\n"); strings.TrimSpace(p) != "" {
			parts = append(parts, span{p, start})
		}
		cur.Reset()
		curLen = 0
	}
	add := func(s string, n int) {
		if curLen == 0 {
			start = pos
		}
		cur.WriteString(s)
		curLen += n
		pos += n
	}
	for line := range strings.SplitAfterSeq(text, "\n") {
		n := utf16Len(line)
		if n > limit {
			flush()
			for _, r := range line {
				if curLen+units(r) > limit {
					flush()
				}
				add(string(r), units(r))
			}
			continue
		}
		if curLen+n > limit {
			flush()
		}
		add(line, n)
	}
	flush()
	return parts
}

type callbackQuery struct {
	ID      string   `json:"id"`
	From    *user    `json:"from"`
	Message *message `json:"message"`
	Data    string   `json:"data"`
}

func normalizeCallback(q *callbackQuery) (bridge.Message, bool) {
	action, id, ok := strings.Cut(q.Data, ":")
	if !ok || len(id) != 32 || (action != "approve" && action != "deny") || q.From == nil || q.Message == nil {
		return bridge.Message{}, false
	}
	text := "/deny"
	if action == "approve" {
		text = "/allow"
	}
	return bridge.Message{Platform: "telegram", ChatID: strconv.FormatInt(q.Message.Chat.ID, 10), SenderID: strconv.FormatInt(q.From.ID, 10), ApprovalID: id, Text: text, Time: time.Now()}, true
}

func (b *Bot) SendApproval(ctx context.Context, chatID, requestID, text string) (func(context.Context, string) error, error) {
	parts := splitText(text, maxUnits-64)
	if len(parts) > 1 {
		if err := b.Send(ctx, chatID, "", strings.Join(parts[:len(parts)-1], "\n"), false); err != nil {
			return nil, err
		}
	}
	var sent message
	err := b.call(ctx, "sendMessage", map[string]any{"chat_id": chatParam(chatID), "text": parts[len(parts)-1], "reply_markup": map[string]any{"inline_keyboard": [][]any{{map[string]any{"text": "Approve", "callback_data": "approve:" + requestID}, map[string]any{"text": "Deny", "callback_data": "deny:" + requestID}}}}}, &sent)
	if err != nil {
		return nil, err
	}
	return func(ctx context.Context, status string) error {
		return b.call(ctx, "editMessageText", map[string]any{"chat_id": chatParam(chatID), "message_id": sent.MessageID, "text": parts[len(parts)-1] + "\n\n" + status, "reply_markup": map[string]any{"inline_keyboard": [][]any{}}}, nil)
	}, nil
}
