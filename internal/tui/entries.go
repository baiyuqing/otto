package tui

import (
	"fmt"
	"strings"

	"github.com/baiyuqing/otto/internal/model"
)

type EntryKind string

const (
	EntryUser      EntryKind = "user"
	EntryAssistant EntryKind = "assistant"
	EntryTool      EntryKind = "tool"
	EntryError     EntryKind = "error"
	EntrySystem    EntryKind = "system"
)

type Entry struct {
	ID          string
	Kind        EntryKind
	Raw         string
	Rendered    string
	RenderWidth int
	ToolCallID  string
	ToolName    string
	ToolArgs    string
	ToolOutput  string
	ToolError   bool
	ToolDone    bool
}

func EntriesFromHistory(history []model.Message) ([]Entry, model.Usage) {
	entries := make([]Entry, 0, len(history))
	pending := make(map[string][]int)
	var usage model.Usage

	for msgIndex, message := range history {
		usage = addUsageTotals(usage, message.Usage)
		baseID := messageEntryBaseID(message, msgIndex)
		textOrdinal := 0
		var text strings.Builder
		flushText := func(force bool) {
			if !force && text.Len() == 0 {
				return
			}
			entries = append(entries, Entry{
				ID:   fmt.Sprintf("%s-text-%d", baseID, textOrdinal),
				Kind: entryKindForRole(message.Role),
				Raw:  text.String(),
			})
			textOrdinal++
			text.Reset()
		}

		if len(message.Blocks) == 0 {
			if message.Role != model.RoleTool {
				flushText(true)
			}
			continue
		}

		for blockIndex, block := range message.Blocks {
			switch block.Type {
			case model.BlockText:
				if message.Role == model.RoleTool {
					flushText(false)
					entries = append(entries, newOrphanToolEntry(baseID, blockIndex, block, true))
					continue
				}
				text.WriteString(block.Text)
			case model.BlockToolCall:
				flushText(false)
				entry := Entry{
					ID:         fmt.Sprintf("%s-tool-%d", baseID, blockIndex),
					Kind:       EntryTool,
					ToolCallID: block.ToolCallID,
					ToolName:   block.ToolName,
					ToolArgs:   string(block.Arguments),
				}
				entries = append(entries, entry)
				if block.ToolCallID != "" {
					pending[block.ToolCallID] = append(pending[block.ToolCallID], len(entries)-1)
				}
			case model.BlockToolResult:
				flushText(false)
				if !pairToolResult(entries, pending, block) {
					entries = append(entries, newOrphanToolEntry(baseID, blockIndex, block, true))
				}
			default:
				if preserveUnknownToolBlock(message.Role, block) {
					flushText(false)
					entries = append(entries, newOrphanToolEntry(baseID, blockIndex, block, block.Text != "" || block.IsError))
					continue
				}
				text.WriteString(block.Text)
			}
		}

		flushText(false)
	}

	return entries, usage
}

func entryKindForRole(role model.Role) EntryKind {
	switch role {
	case model.RoleUser:
		return EntryUser
	case model.RoleAssistant:
		return EntryAssistant
	case model.RoleTool:
		return EntryTool
	default:
		return EntrySystem
	}
}

func messageEntryBaseID(message model.Message, index int) string {
	if message.ID != "" {
		return fmt.Sprintf("message-%d-%s", index, message.ID)
	}
	return fmt.Sprintf("message-%d-%s", index, entryKindForRole(message.Role))
}

func preserveUnknownToolBlock(role model.Role, block model.Block) bool {
	return role == model.RoleTool || block.ToolCallID != "" || block.ToolName != "" || len(block.Arguments) > 0 || block.IsError
}

func newOrphanToolEntry(baseID string, blockIndex int, block model.Block, done bool) Entry {
	return Entry{
		ID:         fmt.Sprintf("%s-tool-%d", baseID, blockIndex),
		Kind:       EntryTool,
		ToolCallID: block.ToolCallID,
		ToolName:   block.ToolName,
		ToolArgs:   string(block.Arguments),
		ToolOutput: block.Text,
		ToolError:  block.IsError,
		ToolDone:   done,
	}
}

func pairToolResult(entries []Entry, pending map[string][]int, block model.Block) bool {
	if block.ToolCallID == "" {
		return false
	}
	indexes := pending[block.ToolCallID]
	if len(indexes) == 0 {
		return false
	}
	entryIndex := indexes[0]
	if len(indexes) == 1 {
		delete(pending, block.ToolCallID)
	} else {
		pending[block.ToolCallID] = indexes[1:]
	}
	entries[entryIndex].ToolOutput = block.Text
	entries[entryIndex].ToolError = block.IsError
	entries[entryIndex].ToolDone = true
	if entries[entryIndex].ToolName == "" {
		entries[entryIndex].ToolName = block.ToolName
	}
	return true
}

func addUsageTotals(total model.Usage, usage *model.Usage) model.Usage {
	if usage == nil {
		return total
	}
	total.InputTokens = saturatingAddNonNegative(total.InputTokens, usage.InputTokens)
	total.OutputTokens = saturatingAddNonNegative(total.OutputTokens, usage.OutputTokens)
	return total
}

func saturatingAddNonNegative(total int, delta int) int {
	if total < 0 {
		total = 0
	}
	if delta <= 0 {
		return total
	}
	limit := maxInt()
	if total >= limit-delta {
		return limit
	}
	return total + delta
}

func maxInt() int {
	return int(^uint(0) >> 1)
}
