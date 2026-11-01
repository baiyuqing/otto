package bridge

import (
	"context"
	"strings"
	"time"
)

// ModelSnapshot is the local Otto configuration the connector shows.
type ModelSnapshot struct {
	// Default is the selector of the configured default model; empty when unset.
	Default string
	// Models is the list of "<provider>/<model_id>" selectors to offer, in the
	// order it should be shown.
	Models []string
}

// ModelSource reads the local Otto configuration's models. The composition
// root injects it; a nil source leaves every "/model" to Otto itself.
type ModelSource interface {
	Models() (ModelSnapshot, error)
}

// Choice is one button of a message: choosing it delivers Command to the bridge
// as a message, exactly as if the sender had typed it.
type Choice struct {
	Label   string
	Command string
}

// ChoicePlatform posts one message whose buttons are the choices. It reports an
// error for a choice list it cannot represent, so the caller can fall back to
// plain text.
type ChoicePlatform interface {
	SendChoices(ctx context.Context, chatID, replyTo, text string, choices []Choice) error
}

// ValidModelSelector reports whether selector may travel as the argument of a
// switch command: "<provider>/<model_id>" of 3 to 128 printable ASCII
// characters, without whitespace, quotes, backslashes or ':'. The provider
// part uses letters, digits, '_' and '-'; the model ID may also hold '.', '+',
// '@' and further '/' (for example "openrouter/anthropic/claude-x").
//
// The point is that a selector stays one command argument: a tapped button or
// card can then never deliver a second word, another option, or anything but a
// model selector.
func ValidModelSelector(selector string) bool {
	provider, modelID, ok := strings.Cut(selector, "/")
	if !ok || provider == "" || modelID == "" || len(selector) > 128 {
		return false
	}
	for _, r := range provider {
		if !isSelectorWord(r) {
			return false
		}
	}
	for _, r := range modelID {
		if !isSelectorWord(r) && r != '.' && r != '+' && r != '@' && r != '/' {
			return false
		}
	}
	return true
}

func isSelectorWord(r rune) bool {
	return r >= 'a' && r <= 'z' || r >= 'A' && r <= 'Z' || r >= '0' && r <= '9' || r == '_' || r == '-'
}

// cmdModel answers bare "/model" with the models this connector can switch
// to, one button each. It returns true when it handled the text.
//
// "/model <selector>" is forwarded to Otto unchanged, and so is bare "/model"
// when no model source is configured: Otto's own "/model" then answers.
func (c *chat) cmdModel(m Message, args string) bool {
	if args != "" || c.b.opts.Models == nil {
		return false
	}
	go c.showModels(m)
	return true
}

func (c *chat) showModels(m Message) {
	ctx, stop := context.WithTimeout(c.b.workCtx, 30*time.Second)
	defer stop()
	snapshot, err := c.b.opts.Models.Models()
	if err != nil {
		c.send(ctx, m.MessageID, "Could not read the local Otto configuration: "+err.Error()+
			". Switch with /model <provider>/<model_id>.")
		return
	}
	if len(snapshot.Models) == 0 {
		c.send(ctx, m.MessageID, "The local Otto configuration lists no models."+
			" Switch with /model <provider>/<model_id>.")
		return
	}
	text := renderModels(snapshot)
	choices := make([]Choice, 0, len(snapshot.Models))
	for _, selector := range snapshot.Models {
		choices = append(choices, Choice{Label: selector, Command: "/model " + selector})
	}
	if platform, ok := c.p.(ChoicePlatform); ok {
		if err := platform.SendChoices(ctx, c.id, m.MessageID, text, choices); err == nil {
			return
		}
	}
	c.send(ctx, m.MessageID, text)
}

// renderModels is the plain-text form of the picker, used by platforms
// without buttons and as the message that carries the buttons.
func renderModels(snapshot ModelSnapshot) string {
	lines := make([]string, 0, len(snapshot.Models)+2)
	lines = append(lines, "Configured models:")
	for _, selector := range snapshot.Models {
		mark := ""
		if snapshot.Default != "" && selector == snapshot.Default {
			mark = " (default)"
		}
		lines = append(lines, "• "+selector+mark)
	}
	lines = append(lines, "Switch with /model <provider>/<model_id>.")
	return strings.Join(lines, "\n")
}
