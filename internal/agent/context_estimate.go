package agent

import (
	"encoding/json"
	"math"

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/provider"
	"github.com/baiyuqing/otto/internal/session"
)

const (
	requestFramingTokens        = 3
	messageFramingTokens        = 6
	textBlockFramingTokens      = 2
	toolCallFramingTokens       = 12
	toolResultFramingTokens     = 8
	toolDefinitionFramingTokens = 16
)

func estimateRequest(request provider.Request, latest session.CompactionMetadata, hasLatest bool) int {
	if anchorIndex, promptTokens, ok := requestUsageAnchor(request.Messages, latest, hasLatest); ok {
		total := promptTokens
		for index := anchorIndex; index < len(request.Messages); index++ {
			total = saturatingEstimateAdd(total, estimateMessage(request.Messages[index]))
		}
		return total
	}

	total := requestFramingTokens
	total = saturatingEstimateAdd(total, estimateString(request.SystemPrompt))
	for _, message := range request.Messages {
		total = saturatingEstimateAdd(total, estimateMessage(message))
	}
	for _, definition := range request.Tools {
		total = saturatingEstimateAdd(total, estimateToolDefinition(definition))
	}
	return total
}

func estimateMessage(message model.Message) int {
	total := messageFramingTokens
	for _, block := range message.Blocks {
		switch block.Type {
		case model.BlockText:
			total = saturatingEstimateAdd(total, textBlockFramingTokens)
			total = saturatingEstimateAdd(total, estimateString(block.Text))
		case model.BlockToolCall:
			total = saturatingEstimateAdd(total, toolCallFramingTokens)
			total = saturatingEstimateAdd(total, estimateString(block.ToolCallID))
			total = saturatingEstimateAdd(total, estimateString(block.ToolName))
			total = saturatingEstimateAdd(total, estimateString(string(block.Arguments)))
		case model.BlockToolResult:
			total = saturatingEstimateAdd(total, toolResultFramingTokens)
			total = saturatingEstimateAdd(total, estimateString(block.Text))
			total = saturatingEstimateAdd(total, estimateString(block.ToolCallID))
			total = saturatingEstimateAdd(total, estimateString(block.ToolName))
		}
	}
	return total
}

func estimateString(value string) int {
	if len(value) == 0 {
		return 0
	}
	return 1 + (len([]byte(value))-1)/3
}

func estimateToolDefinition(definition model.ToolDefinition) int {
	total := toolDefinitionFramingTokens
	total = saturatingEstimateAdd(total, estimateString(definition.Name))
	total = saturatingEstimateAdd(total, estimateString(definition.Description))
	schema, err := json.Marshal(definition.Parameters)
	if err == nil {
		total = saturatingEstimateAdd(total, estimateString(string(schema)))
	}
	return total
}

func requestUsageAnchor(messages []model.Message, latest session.CompactionMetadata, hasLatest bool) (int, int, bool) {
	start := 0
	if hasLatest {
		if latest.FirstPostCheckpointMessageID == "" {
			return 0, 0, false
		}
		found := false
		for index, message := range messages {
			if message.ID == latest.FirstPostCheckpointMessageID {
				start = index
				found = true
				break
			}
		}
		if !found {
			return 0, 0, false
		}
	}
	for index := len(messages) - 1; index >= start; index-- {
		message := messages[index]
		if message.Role != model.RoleAssistant || message.Usage == nil || message.Usage.InputTokens <= 0 {
			continue
		}
		return index, message.Usage.InputTokens, true
	}
	return 0, 0, false
}

func saturatingEstimateAdd(total, delta int) int {
	if total >= math.MaxInt || delta <= 0 {
		if total >= math.MaxInt {
			return math.MaxInt
		}
		return total
	}
	if total > math.MaxInt-delta {
		return math.MaxInt
	}
	return total + delta
}
