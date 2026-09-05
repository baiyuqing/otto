package openairesponses

import (
	"bufio"
	"encoding/json"
	"fmt"
	"io"
	"strings"

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/provider"
)

type streamFailureKind uint8

const (
	streamFailureNone streamFailureKind = iota
	streamFailureRead
	streamFailureProtocol
)

type assembledToolCall struct {
	callID    string
	name      string
	arguments string
}

// readStream consumes the Responses API SSE body, emitting incremental events
// and assembling the final message. The Responses stream terminates with a
// "response.completed" event and then closes; there is no [DONE] sentinel.
func readStream(body io.Reader, emit func(provider.StreamEvent)) (provider.Response, bool, streamFailureKind, error) {
	reader := bufio.NewReader(body)
	var dataLines []string
	var text strings.Builder
	var calls []*assembledToolCall
	byItem := make(map[string]*assembledToolCall)
	var usage model.Usage
	usagePresent := false
	status := ""
	incompleteReason := ""
	emitted := false
	completed := false

	dispatch := func() error {
		if len(dataLines) == 0 {
			return nil
		}
		data := strings.Join(dataLines, "\n")
		dataLines = dataLines[:0]
		var event responsesEvent
		if err := json.Unmarshal([]byte(data), &event); err != nil {
			return fmt.Errorf("decode responses stream: %w", err)
		}
		switch event.Type {
		case "response.output_text.delta":
			if event.Delta != "" {
				text.WriteString(event.Delta)
				emitted = true
				if emit != nil {
					emit(provider.StreamEvent{Type: provider.StreamTextDelta, Text: event.Delta})
				}
			}
		case "response.output_item.added":
			if event.Item != nil && event.Item.Type == "function_call" {
				call := &assembledToolCall{callID: event.Item.CallID, name: event.Item.Name}
				byItem[event.Item.ID] = call
				calls = append(calls, call)
				emitted = true
				if emit != nil {
					emit(provider.StreamEvent{Type: provider.StreamToolCallDelta, ToolCallID: call.callID, ToolName: call.name})
				}
			}
		case "response.function_call_arguments.delta":
			if call := byItem[event.ItemID]; call != nil && event.Delta != "" {
				call.arguments += event.Delta
				emitted = true
				if emit != nil {
					emit(provider.StreamEvent{Type: provider.StreamToolCallDelta, ToolCallID: call.callID, ToolName: call.name, Arguments: event.Delta})
				}
			}
		case "response.output_item.done":
			if event.Item != nil && event.Item.Type == "function_call" {
				if call := byItem[event.Item.ID]; call != nil && call.arguments == "" && event.Item.Arguments != "" {
					call.arguments = event.Item.Arguments
				}
			}
		case "response.completed", "response.incomplete":
			completed = true
			if event.Response != nil {
				status = event.Response.Status
				if event.Response.Usage != nil {
					usagePresent = true
					usage = model.Usage{
						InputTokens:       event.Response.Usage.InputTokens,
						OutputTokens:      event.Response.Usage.OutputTokens,
						CachedInputTokens: event.Response.Usage.cachedTokens(),
					}
				}
				if event.Response.IncompleteDetails != nil {
					incompleteReason = event.Response.IncompleteDetails.Reason
				}
			}
		case "response.failed", "error":
			return fmt.Errorf("responses stream reported %s", event.Type)
		}
		return nil
	}

	for !completed {
		line, err := reader.ReadString('\n')
		if len(line) > 0 {
			line = strings.TrimSuffix(line, "\n")
			line = strings.TrimSuffix(line, "\r")
			switch {
			case line == "":
				if dispatchErr := dispatch(); dispatchErr != nil {
					return provider.Response{}, emitted, streamFailureProtocol, dispatchErr
				}
			case strings.HasPrefix(line, ":"), strings.HasPrefix(line, "event:"):
				// comment or event-name line; the data payload carries type
			case line == "data":
				dataLines = append(dataLines, "")
			case strings.HasPrefix(line, "data:"):
				value := strings.TrimPrefix(line, "data:")
				value = strings.TrimPrefix(value, " ")
				dataLines = append(dataLines, value)
			}
		}
		if err != nil {
			if err != io.EOF {
				return provider.Response{}, emitted, streamFailureRead, err
			}
			if len(dataLines) > 0 {
				if dispatchErr := dispatch(); dispatchErr != nil {
					return provider.Response{}, emitted, streamFailureProtocol, dispatchErr
				}
			}
			break
		}
	}
	if !completed {
		return provider.Response{}, emitted, streamFailureProtocol, fmt.Errorf("responses stream ended without response.completed")
	}

	blocks := make([]model.Block, 0, 1+len(calls))
	if text.Len() > 0 {
		blocks = append(blocks, model.Block{Type: model.BlockText, Text: text.String()})
	}
	for _, call := range calls {
		if !validArguments(call.arguments) {
			return provider.Response{}, emitted, streamFailureProtocol, fmt.Errorf("tool call %q has malformed arguments", call.callID)
		}
		blocks = append(blocks, model.Block{
			Type:       model.BlockToolCall,
			ToolCallID: call.callID,
			ToolName:   call.name,
			Arguments:  json.RawMessage(call.arguments),
		})
	}

	finish := finishReason(len(calls) > 0, status, incompleteReason)
	var messageUsage *model.Usage
	if usagePresent {
		messageUsage = &usage
	}
	return provider.Response{
		Message: model.Message{
			Role:         model.RoleAssistant,
			Blocks:       blocks,
			FinishReason: finish,
			Usage:        messageUsage,
		},
	}, emitted, streamFailureNone, nil
}

func finishReason(hasToolCalls bool, status, incompleteReason string) model.FinishReason {
	if hasToolCalls {
		return model.FinishToolCalls
	}
	if status == "incomplete" && incompleteReason == "max_output_tokens" {
		return model.FinishLength
	}
	return model.FinishStop
}
