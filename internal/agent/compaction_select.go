package agent

import (
	"fmt"
	"strings"

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/session"
)

const compactionSummaryDisplayPrefix = "[Compaction summary]\n"

type compactionSelection struct {
	PreviousSummary  string
	HistoricalSource []model.Message
	TurnPrefixSource []model.Message
	Retained         []model.Message
	FirstKeptID      string
	SplitTurn        bool
}

type compactionMessageGroup struct {
	start   int
	end     int
	primary int
	tokens  int
}

func selectCompaction(
	messages []model.Message,
	latest session.CompactionMetadata,
	hasLatest bool,
	keepRecentTokens int,
	hardInputBudget int,
) (compactionSelection, error) {
	selection := compactionSelection{}
	transcript, previousSummary := compactionTranscript(messages, latest, hasLatest)
	selection.PreviousSummary = previousSummary

	if err := validateRetainedToolPairs(transcript); err != nil {
		return compactionSelection{}, fmt.Errorf("select compaction: %w", err)
	}
	if len(transcript) == 0 {
		return compactionSelection{}, ErrNothingToCompact
	}

	if hasLatest && latest.RetainedTailOnly {
		return selectAfterRetainedTail(transcript, selection, latest, hardInputBudget)
	}

	groups, err := groupCompactionMessages(transcript)
	if err != nil {
		return compactionSelection{}, err
	}
	if len(groups) == 0 {
		return compactionSelection{}, ErrNothingToCompact
	}

	start := latestUserGroup(transcript, groups)
	if start < 0 {
		start = len(groups) - 1
	}
	retainedTokens := groupTokenSum(groups[start:])
	if trailingStart := groups[len(groups)-1].end; trailingStart < len(transcript) {
		retainedTokens = saturatingEstimateAdd(retainedTokens, selectMessageEstimate(transcript[trailingStart:]))
	}
	if hardInputBudget > 0 && retainedTokens > hardInputBudget {
		return compactionSelection{}, ErrCurrentTurnTooLarge
	}

	for start > 0 && retainedTokens < keepRecentTokens {
		candidateTokens := saturatingEstimateAdd(retainedTokens, groups[start-1].tokens)
		if hardInputBudget > 0 && candidateTokens > hardInputBudget {
			break
		}
		start--
		retainedTokens = candidateTokens
	}
	if start == 0 {
		return compactionSelection{}, ErrNothingToCompact
	}

	retainedStart := groups[start].start
	selection.Retained = cloneSelectionMessages(transcript[retainedStart:])
	selection.FirstKeptID = selection.Retained[0].ID
	selection = partitionCompactionSource(transcript, groups, start, retainedStart, selection)

	if len(selection.HistoricalSource) == 0 && len(selection.TurnPrefixSource) == 0 {
		return compactionSelection{}, ErrNothingToCompact
	}
	if err := validateCompactionSelection(selection); err != nil {
		return compactionSelection{}, err
	}
	return selection, nil
}

func selectAfterRetainedTail(
	transcript []model.Message,
	selection compactionSelection,
	latest session.CompactionMetadata,
	hardInputBudget int,
) (compactionSelection, error) {
	if latest.FirstPostCheckpointMessageID == "" {
		return compactionSelection{}, ErrNothingToCompact
	}
	anchor := -1
	for index := range transcript {
		if transcript[index].ID == latest.FirstPostCheckpointMessageID {
			anchor = index
			break
		}
	}
	if anchor < 0 {
		return compactionSelection{}, ErrNothingToCompact
	}
	if hardInputBudget > 0 && selectMessageEstimate(transcript[anchor:]) > hardInputBudget {
		return compactionSelection{}, ErrCurrentTurnTooLarge
	}
	if anchor == 0 {
		return compactionSelection{}, ErrNothingToCompact
	}

	groups, err := groupCompactionMessages(transcript)
	if err != nil {
		return compactionSelection{}, err
	}
	anchorGroup := -1
	for index, group := range groups {
		if group.start == anchor && group.primary == anchor {
			anchorGroup = index
			break
		}
	}
	if anchorGroup < 0 {
		return compactionSelection{}, fmt.Errorf("select compaction: first post-checkpoint message is not a protocol-safe cut")
	}

	selection.Retained = cloneSelectionMessages(transcript[anchor:])
	selection.FirstKeptID = latest.FirstPostCheckpointMessageID
	selection = partitionCompactionSource(transcript, groups, anchorGroup, anchor, selection)
	if err := validateCompactionSelection(selection); err != nil {
		return compactionSelection{}, err
	}
	return selection, nil
}

func partitionCompactionSource(
	transcript []model.Message,
	groups []compactionMessageGroup,
	startGroup int,
	retainedStart int,
	selection compactionSelection,
) compactionSelection {
	if transcript[groups[startGroup].primary].Role == model.RoleAssistant {
		if turnStart := precedingUserGroup(transcript, groups, startGroup); turnStart >= 0 {
			turnStartMessage := groups[turnStart].start
			selection.HistoricalSource = cloneSelectionMessages(transcript[:turnStartMessage])
			selection.TurnPrefixSource = cloneSelectionMessages(transcript[turnStartMessage:retainedStart])
			selection.SplitTurn = len(selection.TurnPrefixSource) != 0
			return selection
		}
	}
	selection.HistoricalSource = cloneSelectionMessages(transcript[:retainedStart])
	return selection
}

func compactionTranscript(messages []model.Message, latest session.CompactionMetadata, hasLatest bool) ([]model.Message, string) {
	transcript := make([]model.Message, 0, len(messages))
	previousSummary := ""
	foundLatest := false
	for _, message := range messages {
		if message.Role != model.RoleContext || message.ContextType != "compaction" {
			transcript = append(transcript, message)
			continue
		}

		isLatest := !hasLatest || latest.ID == "" || message.ID == latest.ID
		if isLatest {
			previousSummary = strings.TrimPrefix(message.Text(), compactionSummaryDisplayPrefix)
			foundLatest = true
		}
	}
	if hasLatest && !foundLatest {
		previousSummary = latest.Summary
	}
	return transcript, previousSummary
}

func groupCompactionMessages(messages []model.Message) ([]compactionMessageGroup, error) {
	groups := make([]compactionMessageGroup, 0, len(messages))
	pendingContext := -1
	for index := 0; index < len(messages); {
		message := messages[index]
		if message.Role == model.RoleContext {
			if pendingContext < 0 {
				pendingContext = index
			}
			index++
			continue
		}
		if message.Role != model.RoleUser && message.Role != model.RoleAssistant {
			return nil, fmt.Errorf("select compaction: tool result is not attached to an assistant call")
		}

		start := index
		if pendingContext >= 0 {
			start = pendingContext
			pendingContext = -1
		}
		end := index + 1
		if message.Role == model.RoleAssistant && messageHasToolCalls(message) {
			for end < len(messages) && messages[end].Role == model.RoleTool {
				end++
			}
		}
		groups = append(groups, compactionMessageGroup{
			start: start, end: end, primary: index, tokens: selectMessageEstimate(messages[start:end]),
		})
		index = end
	}

	return groups, nil
}

func latestUserGroup(messages []model.Message, groups []compactionMessageGroup) int {
	for index := len(groups) - 1; index >= 0; index-- {
		if messages[groups[index].primary].Role == model.RoleUser {
			return index
		}
	}
	return -1
}

func precedingUserGroup(messages []model.Message, groups []compactionMessageGroup, before int) int {
	for index := before - 1; index >= 0; index-- {
		if messages[groups[index].primary].Role == model.RoleUser {
			return index
		}
	}
	return -1
}

func groupTokenSum(groups []compactionMessageGroup) int {
	total := 0
	for _, group := range groups {
		total = saturatingEstimateAdd(total, group.tokens)
	}
	return total
}

func selectMessageEstimate(messages []model.Message) int {
	total := 0
	for _, message := range messages {
		total = saturatingEstimateAdd(total, estimateMessage(message))
	}
	return total
}

func messageHasToolCalls(message model.Message) bool {
	for _, block := range message.Blocks {
		if block.Type == model.BlockToolCall {
			return true
		}
	}
	return false
}

func validateCompactionSelection(selection compactionSelection) error {
	for name, messages := range map[string][]model.Message{
		"historical source":  selection.HistoricalSource,
		"turn-prefix source": selection.TurnPrefixSource,
		"retained context":   selection.Retained,
	} {
		if err := validateRetainedToolPairs(messages); err != nil {
			return fmt.Errorf("select compaction: invalid %s: %w", name, err)
		}
	}
	if len(selection.Retained) == 0 || selection.Retained[0].Role == model.RoleTool {
		return fmt.Errorf("select compaction: retained context has no protocol-safe start")
	}
	if selection.FirstKeptID == "" {
		return ErrNothingToCompact
	}
	return nil
}

func validateRetainedToolPairs(messages []model.Message) error {
	pending := make(map[string]model.Block)
	seen := make(map[string]struct{})
	for _, message := range messages {
		switch message.Role {
		case model.RoleAssistant:
			if len(pending) != 0 {
				return fmt.Errorf("unresolved tool calls must be followed immediately by results")
			}
			for _, block := range message.Blocks {
				if block.Type == model.BlockToolResult {
					return fmt.Errorf("assistant message contains a tool result")
				}
				if block.Type != model.BlockToolCall {
					continue
				}
				if strings.TrimSpace(block.ToolCallID) == "" || strings.TrimSpace(block.ToolName) == "" {
					return fmt.Errorf("assistant tool call requires an id and name")
				}
				if _, duplicate := seen[block.ToolCallID]; duplicate {
					return fmt.Errorf("duplicate tool-call id %q", block.ToolCallID)
				}
				seen[block.ToolCallID] = struct{}{}
				pending[block.ToolCallID] = block
			}
		case model.RoleTool:
			if len(message.Blocks) == 0 {
				return fmt.Errorf("tool message must contain a result")
			}
			for _, block := range message.Blocks {
				if block.Type != model.BlockToolResult {
					return fmt.Errorf("tool message contains a non-result block")
				}
				call, ok := pending[block.ToolCallID]
				if !ok {
					return fmt.Errorf("tool result %q has no pending call", block.ToolCallID)
				}
				if call.ToolName != block.ToolName {
					return fmt.Errorf("tool result %q name does not match its call", block.ToolCallID)
				}
				delete(pending, block.ToolCallID)
			}
		case model.RoleUser, model.RoleContext:
			if len(pending) != 0 {
				return fmt.Errorf("unresolved tool calls must be followed immediately by results")
			}
			for _, block := range message.Blocks {
				if block.Type == model.BlockToolCall || block.Type == model.BlockToolResult {
					return fmt.Errorf("tool blocks are incompatible with %s messages", message.Role)
				}
			}
		default:
			return fmt.Errorf("unsupported message role %q", message.Role)
		}
	}
	if len(pending) != 0 {
		return fmt.Errorf("unresolved tool calls at end of context")
	}
	return nil
}

func cloneSelectionMessages(messages []model.Message) []model.Message {
	return cloneMessages(messages)
}
