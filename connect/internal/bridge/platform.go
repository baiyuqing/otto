package bridge

import (
	"context"
	"time"
)

// Message is one inbound chat message, normalized by a platform adapter.
// Admission (chat and sender allowlists) is checked by the bridge, not here.
type Message struct {
	Platform  string // Platform.Name() of the adapter that delivered it
	ChatID    string
	SenderID  string
	MessageID string
	// Text is the message text with any mention of the bot removed and a
	// command addressed to the bot ("/stop@name") reduced to "/stop".
	ApprovalID string // non-empty only for a button callback; never parsed from chat text
	Text       string
	Time       time.Time // when the platform received the message
	Group      bool
	// Attachment is true when the message carried a photo, file or other
	// non-text content; the adapter drops that content.
	Attachment bool
}

// Platform is one chat service. Implementations must be safe for concurrent
// use by the bridge.
type Platform interface {
	Name() string
	// Run receives messages until ctx ends and passes each to deliver.
	// deliver does not block; when it returns, the message has been queued
	// or rejected and the adapter may acknowledge it to the platform.
	// Run returns nil when ctx ends and an error when the platform cannot
	// be used (for example a rejected token).
	Run(ctx context.Context, deliver func(Message)) error
	// Send posts text to chatID. replyTo is a platform message id or "".
	// The adapter splits text that exceeds the platform's length limit.
	// markdown is true when text is the agent's Markdown reply and false for
	// connector notices, which are plain text.
	Send(ctx context.Context, chatID, replyTo, text string, markdown bool) error
	// Typing shows a typing indicator where the platform supports one.
	Typing(ctx context.Context, chatID string) error
}

// ApprovalPlatform sends buttons bound to one permission request. finish replaces
// them with the final status; it is called once, including on cancellation.
type ApprovalPlatform interface {
	SendApproval(ctx context.Context, chatID, requestID, text string) (finish func(context.Context, string) error, err error)
}
