package tui

import "charm.land/bubbles/v2/key"

type KeyMap struct {
	Submit         key.Binding
	InsertNewline  key.Binding
	Cancel         key.Binding
	ToggleTools    key.Binding
	Help           key.Binding
	Session        key.Binding
	Complete       key.Binding
	SuggestionUp   key.Binding
	SuggestionDown key.Binding
	PageUp         key.Binding
	PageDown       key.Binding
	Home           key.Binding
	End            key.Binding
	ResumeUp       key.Binding
	ResumeDown     key.Binding
	ResumePageUp   key.Binding
	ResumePageDown key.Binding
	ResumeSelect   key.Binding
	ResumeClose    key.Binding
}

func DefaultKeyMap() KeyMap {
	return KeyMap{
		Submit:         key.NewBinding(key.WithKeys("enter"), key.WithHelp("enter", "submit")),
		InsertNewline:  key.NewBinding(key.WithKeys("alt+enter", "shift+enter"), key.WithHelp("alt+enter", "newline")),
		Cancel:         key.NewBinding(key.WithKeys("esc"), key.WithHelp("esc", "cancel")),
		ToggleTools:    key.NewBinding(key.WithKeys("ctrl+o"), key.WithHelp("ctrl+o", "toggle tool output")),
		Help:           key.NewBinding(key.WithKeys("?"), key.WithHelp("?", "help")),
		Session:        key.NewBinding(key.WithKeys("/session"), key.WithHelp("/session", "session info")),
		Complete:       key.NewBinding(key.WithKeys("tab"), key.WithHelp("tab", "complete command")),
		SuggestionUp:   key.NewBinding(key.WithKeys("up"), key.WithHelp("up", "previous suggestion")),
		SuggestionDown: key.NewBinding(key.WithKeys("down"), key.WithHelp("down", "next suggestion")),
		PageUp:         key.NewBinding(key.WithKeys("pgup"), key.WithHelp("pgup", "page up")),
		PageDown:       key.NewBinding(key.WithKeys("pgdown"), key.WithHelp("pgdown", "page down")),
		Home:           key.NewBinding(key.WithKeys("home"), key.WithHelp("home", "top")),
		End:            key.NewBinding(key.WithKeys("end"), key.WithHelp("end", "bottom")),
		ResumeUp:       key.NewBinding(key.WithKeys("up"), key.WithHelp("up", "previous session")),
		ResumeDown:     key.NewBinding(key.WithKeys("down"), key.WithHelp("down", "next session")),
		ResumePageUp:   key.NewBinding(key.WithKeys("pgup"), key.WithHelp("pgup", "previous page")),
		ResumePageDown: key.NewBinding(key.WithKeys("pgdown"), key.WithHelp("pgdown", "next page")),
		ResumeSelect:   key.NewBinding(key.WithKeys("enter"), key.WithHelp("enter", "resume session")),
		ResumeClose:    key.NewBinding(key.WithKeys("esc"), key.WithHelp("esc", "close")),
	}
}
