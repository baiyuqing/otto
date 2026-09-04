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
	slashCommandModel
	slashCommandResume
	slashCommandArchive
	slashCommandCompact
	slashCommandMemory
	slashCommandRemember
	slashCommandLogin
	slashCommandLogout
	slashCommandExit
	slashCommandTasks
	slashCommandTask
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
	{Name: "/model", Description: "show current model, or switch profiles (fresh session)", Kind: slashCommandModel},
	{Name: "/resume", Description: "resume a session", Kind: slashCommandResume},
	{Name: "/archive", Description: "archive a session", Kind: slashCommandArchive},
	{Name: "/compact", Description: "compact context", Kind: slashCommandCompact},
	{Name: "/memory", Description: "search, forget, or review remembered records", Kind: slashCommandMemory},
	{Name: "/remember", Description: "remember a fact for later", Kind: slashCommandRemember},
	{Name: "/login", Description: "sign in to ChatGPT (add status to check)", Kind: slashCommandLogin},
	{Name: "/logout", Description: "sign out of ChatGPT", Kind: slashCommandLogout},
	{Name: "/exit", Description: "quit", Kind: slashCommandExit},
	{Name: "/tasks", Description: "list sub-agent tasks", Kind: slashCommandTasks},
	{Name: "/task", Description: "show or cancel a sub-agent task", Kind: slashCommandTask},
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
