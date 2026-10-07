package bridge

import (
	"context"
	"fmt"
	"strings"
	"time"
)

// ProfileOption is one profile a chat may switch to.
type ProfileOption struct {
	Name     string
	Provider string
	Model    string
}

// ProfileSnapshot is the local Otto configuration the connector shows.
type ProfileSnapshot struct {
	// Default is the configured default_profile; empty when unset.
	Default string
	// Profiles is the list to offer, in the order it should be shown.
	Profiles []ProfileOption
}

// ProfileSource reads the local Otto configuration's profiles. The composition
// root injects it; a nil source leaves every "/model" to Otto itself.
type ProfileSource interface {
	Profiles() (ProfileSnapshot, error)
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

// ValidProfileArg reports whether name may travel as the argument of a switch
// command: 1 to 64 printable ASCII characters, without whitespace, quotes,
// backslashes, '/' or ':'.
//
// Otto accepts any hand-written profile name, including one with dots such as
// "chatgpt-5.5", so this is deliberately wider than Otto's own editing rule.
// The point is that a name stays one command argument: a tapped button or card
// can then never deliver a second word, another option, or anything but a
// profile name.
func ValidProfileArg(name string) bool {
	if name == "" || len(name) > 64 {
		return false
	}
	for _, r := range name {
		switch {
		case r >= 'a' && r <= 'z', r >= 'A' && r <= 'Z', r >= '0' && r <= '9':
		case r == '_', r == '-', r == '.', r == '+', r == '@':
		default:
			return false
		}
	}
	return true
}

// cmdModel answers bare "/model" with the profiles this connector can switch
// to, one button each. It returns true when it handled the text.
//
// "/model <profile>" is forwarded to Otto unchanged, and so is bare "/model"
// when no profile source is configured: Otto's own "/model" then answers.
func (c *chat) cmdModel(m Message, args string) bool {
	if args != "" || c.b.opts.Profiles == nil {
		return false
	}
	go c.showProfiles(m)
	return true
}

func (c *chat) showProfiles(m Message) {
	ctx, stop := context.WithTimeout(c.b.workCtx, 30*time.Second)
	defer stop()
	snapshot, err := c.b.opts.Profiles.Profiles()
	if err != nil {
		c.send(ctx, m.MessageID, "Could not read the local Otto configuration: "+err.Error()+
			". Send /model <profile> to switch directly.")
		return
	}
	if len(snapshot.Profiles) == 0 {
		c.send(ctx, m.MessageID, "The local Otto configuration lists no profiles."+
			" Send /model <profile> to switch directly.")
		return
	}
	text := renderProfiles(snapshot)
	choices := make([]Choice, 0, len(snapshot.Profiles))
	for _, profile := range snapshot.Profiles {
		choices = append(choices, Choice{Label: profile.Name, Command: "/model " + profile.Name})
	}
	if platform, ok := c.p.(ChoicePlatform); ok {
		if err := platform.SendChoices(ctx, c.id, m.MessageID, text, choices); err == nil {
			return
		}
	}
	c.send(ctx, m.MessageID, text)
}

// renderProfiles is the plain-text form of the picker, used by platforms
// without buttons and as the message that carries the buttons.
func renderProfiles(snapshot ProfileSnapshot) string {
	lines := make([]string, 0, len(snapshot.Profiles)+2)
	lines = append(lines, "Profiles:")
	for _, profile := range snapshot.Profiles {
		mark := ""
		if snapshot.Default != "" && profile.Name == snapshot.Default {
			mark = " (default)"
		}
		lines = append(lines, fmt.Sprintf("• %s — %s / %s%s", profile.Name, profile.Provider, profile.Model, mark))
	}
	lines = append(lines, "Send /model <profile> to switch this conversation.")
	return strings.Join(lines, "\n")
}
