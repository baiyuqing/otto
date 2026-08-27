package tui

import "charm.land/bubbles/v2/key"

type KeyMap struct {
	Submit        key.Binding
	InsertNewline key.Binding
	Cancel        key.Binding
	ToggleTools   key.Binding
	Help          key.Binding
	Session       key.Binding
	PageUp        key.Binding
	PageDown      key.Binding
	Home          key.Binding
	End           key.Binding
}

func DefaultKeyMap() KeyMap {
	return KeyMap{
		Submit:        key.NewBinding(key.WithKeys("enter"), key.WithHelp("enter", "submit")),
		InsertNewline: key.NewBinding(key.WithKeys("shift+enter", "alt+enter"), key.WithHelp("alt+enter", "newline")),
		Cancel:        key.NewBinding(key.WithKeys("esc", "ctrl+c"), key.WithHelp("esc", "cancel")),
		ToggleTools:   key.NewBinding(key.WithKeys("ctrl+o"), key.WithHelp("ctrl+o", "toggle tool output")),
		Help:          key.NewBinding(key.WithKeys("?"), key.WithHelp("?", "help")),
		Session:       key.NewBinding(key.WithKeys("/session"), key.WithHelp("/session", "session info")),
		PageUp:        key.NewBinding(key.WithKeys("pgup"), key.WithHelp("pgup", "page up")),
		PageDown:      key.NewBinding(key.WithKeys("pgdown"), key.WithHelp("pgdown", "page down")),
		Home:          key.NewBinding(key.WithKeys("home"), key.WithHelp("home", "top")),
		End:           key.NewBinding(key.WithKeys("end"), key.WithHelp("end", "bottom")),
	}
}
