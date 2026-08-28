package tui

import "strings"

type slashCommandKind uint8

const (
	slashCommandHelp slashCommandKind = iota
	slashCommandSession
	slashCommandNew
	slashCommandResume
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
