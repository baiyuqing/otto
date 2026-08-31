// Package command defines Otto's slash commands and prefix matching.
package command

import (
	"strings"
	"unicode"
)

// Kind identifies which slash command a Command represents.
type Kind uint8

const (
	KindHelp Kind = iota
	KindSession
	KindNew
	KindResume
	KindArchive
	KindCompact
	KindMemory
	KindRemember
	KindExit
)

// Command is a single slash command definition.
type Command struct {
	Name        string
	Description string
	Kind        Kind
}

// Commands is the list of all known slash commands.
var Commands = []Command{
	{Name: "/help", Description: "show help", Kind: KindHelp},
	{Name: "/session", Description: "show session details", Kind: KindSession},
	{Name: "/new", Description: "start a new session", Kind: KindNew},
	{Name: "/resume", Description: "resume a session", Kind: KindResume},
	{Name: "/archive", Description: "archive a session", Kind: KindArchive},
	{Name: "/compact", Description: "compact context", Kind: KindCompact},
	{Name: "/memory", Description: "search, forget, or review remembered records", Kind: KindMemory},
	{Name: "/remember", Description: "remember a fact for later", Kind: KindRemember},
	{Name: "/exit", Description: "quit", Kind: KindExit},
}

// Match returns the commands whose name starts with value.
func Match(value string) []Command {
	if value == "" || value[0] != '/' || strings.ContainsAny(value, "\r\n") {
		return nil
	}
	matches := make([]Command, 0, len(Commands))
	for _, c := range Commands {
		if strings.HasPrefix(c.Name, value) {
			matches = append(matches, c)
		}
	}
	return matches
}

// Find returns the command with the given name.
func Find(name string) (Command, bool) {
	for _, c := range Commands {
		if c.Name == name {
			return c, true
		}
	}
	return Command{}, false
}

// Parse splits value into a command and its trailing argument.
func Parse(value string) (Command, string, bool) {
	value = strings.TrimSpace(value)
	name := value
	argument := ""
	if index := strings.IndexFunc(value, unicode.IsSpace); index >= 0 {
		name = value[:index]
		argument = strings.TrimSpace(value[index:])
	}
	c, ok := Find(name)
	return c, argument, ok
}
