package tui

import (
	"strings"
	"unicode"
)

type slashCommandKind uint8

const (
	slashCommandHelp slashCommandKind = iota
	slashCommandSession
	slashCommandNew
	slashCommandResume
	slashCommandCompact
	slashCommandExit
)

type slashCommand struct {
	Name        string
	Description string
	Kind        slashCommandKind
}

var slashCommands = []slashCommand{
	{Name: "/help", Description: "show help", Kind: slashCommandHelp},
	{Name: "/session", Description: "show session details", Kind: slashCommandSession},
	{Name: "/new", Description: "start a new session", Kind: slashCommandNew},
	{Name: "/resume", Description: "resume a session", Kind: slashCommandResume},
	{Name: "/compact", Description: "compact context", Kind: slashCommandCompact},
	{Name: "/exit", Description: "quit", Kind: slashCommandExit},
}

func matchingSlashCommands(value string) []slashCommand {
	if value == "" || value[0] != '/' || strings.ContainsAny(value, "\r\n") {
		return nil
	}
	matches := make([]slashCommand, 0, len(slashCommands))
	for _, command := range slashCommands {
		if strings.HasPrefix(command.Name, value) {
			matches = append(matches, command)
		}
	}
	return matches
}

func findSlashCommand(name string) (slashCommand, bool) {
	for _, command := range slashCommands {
		if command.Name == name {
			return command, true
		}
	}
	return slashCommand{}, false
}

func parseSlashCommand(value string) (slashCommand, string, bool) {
	value = strings.TrimSpace(value)
	name := value
	argument := ""
	if index := strings.IndexFunc(value, unicode.IsSpace); index >= 0 {
		name = value[:index]
		argument = strings.TrimSpace(value[index:])
	}
	command, ok := findSlashCommand(name)
	return command, argument, ok
}
