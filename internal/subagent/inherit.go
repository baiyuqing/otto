package subagent

import "github.com/baiyuqing/otto/internal/model"

// InheritSnapshot returns the prefix of messages a child with context
// "inherit" receives: everything before the last assistant message (the one
// carrying the pending agent tool call(s)); tool results appended after it
// for sibling calls are cut too. No assistant message → nil.
func InheritSnapshot(messages []model.Message) []model.Message {
	lastAssistant := -1
	for i := len(messages) - 1; i >= 0; i-- {
		if messages[i].Role == model.RoleAssistant {
			lastAssistant = i
			break
		}
	}
	if lastAssistant == -1 {
		return nil
	}
	prefix := messages[:lastAssistant]
	out := make([]model.Message, len(prefix))
	copy(out, prefix)
	return out
}
