package session

import (
	"fmt"
	"math"
	"regexp"
	"strconv"
	"strings"
	"time"

	"github.com/baiyuqing/otto/internal/model"
)

const (
	compactionContextType = "compaction"
	branchContextType     = "branch_summary"
	maxContextWarnings    = 32
	maxWarningTypeBytes   = 32
)

var warningTypeCharacter = regexp.MustCompile(`[A-Za-z0-9._-]`)

type ResolvedContext struct {
	Messages      []model.Message
	Runtime       RuntimeMetadata
	Usage         model.Usage
	UsagePresent  bool
	SessionName   string
	ThinkingLevel string
}

type contextEntryIndex struct {
	byID    map[string]piEntry
	indexes map[string]int
}

type contextWarningCollector struct {
	warnings []Warning
	omitted  bool
}

func buildContext(entries []piEntry, leafID string) (ResolvedContext, []Warning, error) {
	index, warnings, err := indexContextEntries(entries)
	if err != nil {
		return ResolvedContext{}, nil, err
	}
	collector := contextWarningCollector{warnings: warnings}

	path, err := activeContextPath(entries, leafID, index)
	if err != nil {
		return ResolvedContext{}, collector.warnings, err
	}

	resolved := ResolvedContext{ThinkingLevel: "off"}
	var latestRuntime, latestModelChange, latestAssistant *RuntimeMetadata
	for _, entry := range path {
		switch entry.Type {
		case "message":
			if entry.Message == nil {
				return ResolvedContext{}, collector.warnings, fmt.Errorf("%w: message payload is required", ErrInvalidSession)
			}
			if entry.Message.Role == "assistant" {
				latestAssistant = &RuntimeMetadata{Provider: entry.Message.Provider, Model: entry.Message.Model}
				usage, err := piUsageToModel(entry.Message.Usage)
				if err != nil {
					return ResolvedContext{}, collector.warnings, err
				}
				resolved.Usage = addResolvedUsage(resolved.Usage, usage)
				resolved.UsagePresent = true
			}
		case "model_change":
			if entry.ModelChange == nil {
				return ResolvedContext{}, collector.warnings, fmt.Errorf("%w: model_change payload is required", ErrInvalidSession)
			}
			latestModelChange = &RuntimeMetadata{Provider: entry.ModelChange.Provider, Model: entry.ModelChange.ModelID}
		case "thinking_level_change":
			if entry.ThinkingLevelChange == nil {
				return ResolvedContext{}, collector.warnings, fmt.Errorf("%w: thinking_level_change payload is required", ErrInvalidSession)
			}
			resolved.ThinkingLevel = entry.ThinkingLevelChange.ThinkingLevel
		case "compaction":
			if entry.Compaction == nil {
				return ResolvedContext{}, collector.warnings, fmt.Errorf("%w: compaction payload is required", ErrInvalidSession)
			}
			usage, err := optionalPiUsageToModel(entry.Compaction.Usage)
			if err != nil {
				return ResolvedContext{}, collector.warnings, err
			}
			resolved.Usage = addResolvedUsage(resolved.Usage, usage)
			if entry.Compaction.Usage != nil {
				resolved.UsagePresent = true
			}
		case "branch_summary":
			if entry.BranchSummary == nil {
				return ResolvedContext{}, collector.warnings, fmt.Errorf("%w: branch_summary payload is required", ErrInvalidSession)
			}
			usage, err := optionalPiUsageToModel(entry.BranchSummary.Usage)
			if err != nil {
				return ResolvedContext{}, collector.warnings, err
			}
			resolved.Usage = addResolvedUsage(resolved.Usage, usage)
			if entry.BranchSummary.Usage != nil {
				resolved.UsagePresent = true
			}
		case "custom":
			if entry.Custom == nil {
				return ResolvedContext{}, collector.warnings, fmt.Errorf("%w: custom payload is required", ErrInvalidSession)
			}
			if entry.Custom.CustomType == ottoRuntimeCustomType {
				metadata, err := decodeRuntimeMetadata(entry.Custom.Data)
				if err != nil {
					return ResolvedContext{}, collector.warnings, err
				}
				latestRuntime = &metadata
			}
		case "session_info":
			if entry.SessionInfo == nil {
				return ResolvedContext{}, collector.warnings, fmt.Errorf("%w: session_info payload is required", ErrInvalidSession)
			}
			resolved.SessionName = ""
			if entry.SessionInfo.Name != nil {
				resolved.SessionName = *entry.SessionInfo.Name
			}
		default:
			if !knownPiEntryType(entry.Type) {
				collector.add(fmt.Sprintf("ignored unknown active session entry %s of type %s", entry.ID, sanitizeWarningType(entry.Type)))
			}
		}
	}

	switch {
	case latestRuntime != nil:
		resolved.Runtime = *latestRuntime
	case latestModelChange != nil:
		resolved.Runtime = *latestModelChange
	case latestAssistant != nil:
		resolved.Runtime = *latestAssistant
	}

	selected, err := compactionAwarePath(path)
	if err != nil {
		return ResolvedContext{}, collector.warnings, err
	}
	for _, entry := range selected {
		messages, err := piEntryToContextMessages(entry)
		if err != nil {
			return ResolvedContext{}, collector.warnings, err
		}
		resolved.Messages = append(resolved.Messages, messages...)
	}
	if _, err := pendingToolCalls(resolved.Messages); err != nil {
		return ResolvedContext{}, collector.warnings, err
	}
	return resolved, collector.warnings, nil
}

func indexContextEntries(entries []piEntry) (contextEntryIndex, []Warning, error) {
	index := contextEntryIndex{
		byID:    make(map[string]piEntry, len(entries)),
		indexes: make(map[string]int, len(entries)),
	}
	collector := contextWarningCollector{}
	for position, entry := range entries {
		if err := validateContextEntryBase(entry); err != nil {
			return contextEntryIndex{}, nil, fmt.Errorf("session entry %d: %w", position+1, err)
		}
		if _, duplicate := index.byID[entry.ID]; duplicate {
			return contextEntryIndex{}, nil, fmt.Errorf("session entry %d: %w: duplicate entry id", position+1, ErrInvalidSession)
		}
		index.byID[entry.ID] = entry
		index.indexes[entry.ID] = position
	}

	rootCount := 0
	for position, entry := range entries {
		if entry.ParentID == nil {
			rootCount++
			continue
		}
		parentPosition, exists := index.indexes[*entry.ParentID]
		if !exists {
			rootCount++
			collector.add(fmt.Sprintf("session entry %s has missing parent %s; treating entry as a root", entry.ID, *entry.ParentID))
			continue
		}
		if *entry.ParentID == entry.ID {
			return contextEntryIndex{}, nil, fmt.Errorf("session entry %d: %w: parent cycle", position+1, ErrInvalidSession)
		}
		if parentPosition >= position {
			return contextEntryIndex{}, nil, fmt.Errorf("session entry %d: %w: parent id does not reference a prior entry", position+1, ErrInvalidSession)
		}
	}
	if rootCount > 1 {
		collector.add("session contains multiple roots")
	}
	return index, collector.warnings, nil
}

func validateContextEntryBase(entry piEntry) error {
	if strings.TrimSpace(entry.Type) == "" {
		return fmt.Errorf("%w: entry type is required", ErrInvalidSession)
	}
	if !piEntryIDPattern.MatchString(entry.ID) {
		return fmt.Errorf("%w: entry id must be eight lowercase hexadecimal characters", ErrInvalidSession)
	}
	if _, err := time.Parse(time.RFC3339Nano, entry.Timestamp); err != nil {
		return fmt.Errorf("%w: entry timestamp is invalid", ErrInvalidSession)
	}
	if entry.ParentID != nil && !piEntryIDPattern.MatchString(*entry.ParentID) {
		return fmt.Errorf("%w: parent id must be eight lowercase hexadecimal characters", ErrInvalidSession)
	}
	return nil
}

func activeContextPath(entries []piEntry, leafID string, index contextEntryIndex) ([]piEntry, error) {
	if len(entries) == 0 {
		if leafID != "" {
			return nil, fmt.Errorf("%w: active leaf does not exist", ErrInvalidSession)
		}
		return nil, nil
	}
	current, exists := index.byID[leafID]
	if !exists {
		return nil, fmt.Errorf("%w: active leaf does not exist", ErrInvalidSession)
	}
	visited := make(map[string]struct{})
	path := make([]piEntry, 0)
	for {
		if _, duplicate := visited[current.ID]; duplicate {
			return nil, fmt.Errorf("%w: active branch contains a cycle", ErrInvalidSession)
		}
		visited[current.ID] = struct{}{}
		path = append(path, current)
		if current.ParentID == nil {
			break
		}
		parent, exists := index.byID[*current.ParentID]
		if !exists {
			break
		}
		current = parent
	}
	for left, right := 0, len(path)-1; left < right; left, right = left+1, right-1 {
		path[left], path[right] = path[right], path[left]
	}
	return path, nil
}

func compactionAwarePath(path []piEntry) ([]piEntry, error) {
	latest := -1
	for index, entry := range path {
		if entry.Type == "compaction" {
			latest = index
		}
	}
	if latest < 0 {
		return path, nil
	}
	compaction := path[latest]
	if compaction.Compaction == nil {
		return nil, fmt.Errorf("%w: compaction payload is required", ErrInvalidSession)
	}
	if compactionHasRetainedTail(compaction.Compaction) {
		selected := make([]piEntry, 0, 1+len(path)-latest-1)
		selected = append(selected, compaction)
		selected = append(selected, path[latest+1:]...)
		return selected, nil
	}
	if compaction.Compaction.FirstKeptEntryID == nil {
		return nil, fmt.Errorf("%w: compaction requires firstKeptEntryId or retainedTail", ErrInvalidSession)
	}
	firstKept := -1
	for index := 0; index < latest; index++ {
		if path[index].ID == *compaction.Compaction.FirstKeptEntryID {
			firstKept = index
			break
		}
	}
	if firstKept < 0 {
		return nil, fmt.Errorf("%w: compaction firstKeptEntryId is not on the active path before the checkpoint", ErrInvalidSession)
	}
	selected := make([]piEntry, 0, 1+latest-firstKept+len(path)-latest-1)
	selected = append(selected, compaction)
	selected = append(selected, path[firstKept:latest]...)
	selected = append(selected, path[latest+1:]...)
	return selected, nil
}

func piEntryToContextMessages(entry piEntry) ([]model.Message, error) {
	createdAt, err := time.Parse(time.RFC3339Nano, entry.Timestamp)
	if err != nil {
		return nil, fmt.Errorf("%w: entry timestamp is invalid", ErrInvalidSession)
	}
	switch entry.Type {
	case "message":
		if entry.Message == nil {
			return nil, fmt.Errorf("%w: message payload is required", ErrInvalidSession)
		}
		message, err := piMessageToContextMessage(entry.Message, entry.ID, createdAt)
		if err != nil {
			return nil, err
		}
		return []model.Message{message}, nil
	case "compaction":
		if entry.Compaction == nil {
			return nil, fmt.Errorf("%w: compaction payload is required", ErrInvalidSession)
		}
		usage, err := optionalPiUsageToModel(entry.Compaction.Usage)
		if err != nil {
			return nil, err
		}
		messages := []model.Message{newContextMessage(entry.ID, compactionContextType, true, "[Compaction summary]\n"+entry.Compaction.Summary, createdAt, usage)}
		if compactionHasRetainedTail(entry.Compaction) {
			for index := range entry.Compaction.RetainedTail {
				wire := &entry.Compaction.RetainedTail[index]
				message, err := piMessageToContextMessage(wire, entry.ID+"-tail-"+strconv.Itoa(index), time.UnixMilli(wire.Timestamp).UTC())
				if err != nil {
					return nil, err
				}
				messages = append(messages, message)
			}
		}
		return messages, nil
	case "branch_summary":
		if entry.BranchSummary == nil {
			return nil, fmt.Errorf("%w: branch_summary payload is required", ErrInvalidSession)
		}
		usage, err := optionalPiUsageToModel(entry.BranchSummary.Usage)
		if err != nil {
			return nil, err
		}
		return []model.Message{newContextMessage(entry.ID, branchContextType, true, "[Branch summary]\n"+entry.BranchSummary.Summary, createdAt, usage)}, nil
	case "custom_message":
		if entry.CustomMessage == nil {
			return nil, fmt.Errorf("%w: custom_message payload is required", ErrInvalidSession)
		}
		text, err := piContextText(entry.CustomMessage.ContentText, entry.CustomMessage.ContentBlocks)
		if err != nil {
			return nil, err
		}
		return []model.Message{newContextMessage(entry.ID, entry.CustomMessage.CustomType, entry.CustomMessage.Display, customContextText(entry.CustomMessage.CustomType, text), createdAt, nil)}, nil
	default:
		return nil, nil
	}
}

func piMessageToContextMessage(wire *piMessage, id string, createdAt time.Time) (model.Message, error) {
	message := model.Message{ID: id, CreatedAt: createdAt}
	switch wire.Role {
	case "user":
		blocks, err := piContextTextAndToolBlocks(wire, model.RoleUser)
		if err != nil {
			return model.Message{}, err
		}
		if len(blocks) == 0 {
			return model.Message{}, fmt.Errorf("%w: user message content is required", ErrInvalidSession)
		}
		message.Role = model.RoleUser
		message.Blocks = blocks
	case "assistant":
		blocks, err := piContextTextAndToolBlocks(wire, model.RoleAssistant)
		if err != nil {
			return model.Message{}, err
		}
		finishReason, err := piStopReasonToModel(wire.StopReason)
		if err != nil {
			return model.Message{}, err
		}
		if err := validateAssistantToolFinish(blocks, finishReason); err != nil {
			return model.Message{}, err
		}
		usage, err := piUsageToModel(wire.Usage)
		if err != nil {
			return model.Message{}, err
		}
		message.Role = model.RoleAssistant
		message.Blocks = blocks
		message.FinishReason = finishReason
		message.Usage = usage
	case "toolResult":
		text, err := piContextText(wire.ContentText, wire.ContentBlocks)
		if err != nil {
			return model.Message{}, err
		}
		if strings.TrimSpace(wire.ToolCallID) == "" || strings.TrimSpace(wire.ToolName) == "" || wire.IsError == nil {
			return model.Message{}, fmt.Errorf("%w: tool-result message is malformed", ErrInvalidSession)
		}
		message.Role = model.RoleTool
		message.Blocks = []model.Block{{Type: model.BlockToolResult, Text: text, ToolCallID: wire.ToolCallID, ToolName: wire.ToolName, IsError: *wire.IsError}}
	case "custom":
		text, err := piContextText(wire.ContentText, wire.ContentBlocks)
		if err != nil {
			return model.Message{}, err
		}
		display := wire.Display != nil && *wire.Display
		return newContextMessage(id, wire.CustomType, display, customContextText(wire.CustomType, text), createdAt, nil), nil
	case "branchSummary":
		return newContextMessage(id, branchContextType, true, "[Branch summary]\n"+wire.Summary, createdAt, nil), nil
	case "compactionSummary":
		return newContextMessage(id, compactionContextType, true, "[Compaction summary]\n"+wire.Summary, createdAt, nil), nil
	default:
		return model.Message{}, fmt.Errorf("%w: Pi message role is not supported by Otto", ErrUnsupportedSessionContent)
	}
	return message, nil
}

func piContextTextAndToolBlocks(message *piMessage, role model.Role) ([]model.Block, error) {
	if message.ContentText != nil {
		return []model.Block{{Type: model.BlockText, Text: *message.ContentText}}, nil
	}
	blocks := make([]model.Block, 0, len(message.ContentBlocks))
	for _, block := range message.ContentBlocks {
		switch block.Type {
		case "text":
			if block.TextSignature != "" {
				return nil, fmt.Errorf("%w: provider-specific text signatures are not supported", ErrUnsupportedSessionContent)
			}
			blocks = append(blocks, model.Block{Type: model.BlockText, Text: block.Text})
		case "toolCall":
			if role != model.RoleAssistant {
				return nil, fmt.Errorf("%w: tool call is incompatible with message role", ErrUnsupportedSessionContent)
			}
			if strings.TrimSpace(block.ID) == "" || strings.TrimSpace(block.Name) == "" || !validToolArguments(block.Arguments) {
				return nil, fmt.Errorf("%w: tool-call shape cannot be represented", ErrUnsupportedSessionContent)
			}
			if block.ThoughtSignature != "" || block.Namespace != "" {
				return nil, fmt.Errorf("%w: provider-specific tool-call content is not supported", ErrUnsupportedSessionContent)
			}
			blocks = append(blocks, model.Block{Type: model.BlockToolCall, ToolCallID: block.ID, ToolName: block.Name, Arguments: cloneRaw(block.Arguments)})
		case "image", "thinking":
			return nil, fmt.Errorf("%w: Pi message content is not supported by Otto", ErrUnsupportedSessionContent)
		default:
			return nil, fmt.Errorf("%w: Pi message content type is unsupported", ErrUnsupportedSessionContent)
		}
	}
	return blocks, nil
}

func piContextText(contentText *string, blocks []piContentBlock) (string, error) {
	if contentText != nil {
		return *contentText, nil
	}
	var text strings.Builder
	for _, block := range blocks {
		if block.Type != "text" || block.TextSignature != "" {
			return "", fmt.Errorf("%w: context content must be plain text", ErrUnsupportedSessionContent)
		}
		text.WriteString(block.Text)
	}
	return text.String(), nil
}

func newContextMessage(id, contextType string, display bool, text string, createdAt time.Time, usage *model.Usage) model.Message {
	return model.Message{
		ID: id, Role: model.RoleContext, Blocks: []model.Block{{Type: model.BlockText, Text: text}}, CreatedAt: createdAt,
		Usage: usage, ContextType: contextType, Display: display,
	}
}

func customContextText(customType, text string) string {
	return "[Custom context: " + customType + "]\n" + text
}

func optionalPiUsageToModel(usage *piUsage) (*model.Usage, error) {
	if usage == nil {
		return nil, nil
	}
	return piUsageToModel(usage)
}

func addResolvedUsage(total model.Usage, usage *model.Usage) model.Usage {
	if usage == nil {
		return total
	}
	total.InputTokens = saturatingUsageAdd(total.InputTokens, usage.InputTokens)
	total.OutputTokens = saturatingUsageAdd(total.OutputTokens, usage.OutputTokens)
	return total
}

func saturatingUsageAdd(total, delta int) int {
	if delta <= 0 {
		return total
	}
	if total > math.MaxInt-delta {
		return math.MaxInt
	}
	return total + delta
}

func compactionHasRetainedTail(compaction *piCompaction) bool {
	return compaction.RetainedTail != nil
}

func knownPiEntryType(entryType string) bool {
	switch entryType {
	case "message", "model_change", "thinking_level_change", "compaction", "branch_summary", "custom", "custom_message", "label", "session_info":
		return true
	default:
		return false
	}
}

func sanitizeWarningType(entryType string) string {
	var sanitized strings.Builder
	for _, character := range entryType {
		if sanitized.Len() >= maxWarningTypeBytes {
			break
		}
		text := string(character)
		if warningTypeCharacter.MatchString(text) {
			sanitized.WriteRune(character)
		} else {
			sanitized.WriteByte('?')
		}
	}
	if sanitized.Len() == 0 {
		return "unknown"
	}
	return sanitized.String()
}

func (collector *contextWarningCollector) add(message string) {
	if collector.omitted {
		return
	}
	if len(collector.warnings) < maxContextWarnings {
		collector.warnings = append(collector.warnings, Warning{Message: message})
		return
	}
	collector.warnings[maxContextWarnings-1] = Warning{Message: "additional session warnings omitted"}
	collector.omitted = true
}
