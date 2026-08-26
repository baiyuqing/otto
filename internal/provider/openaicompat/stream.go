package openaicompat

import (
	"bufio"
	"encoding/json"
	"fmt"
	"io"
	"strings"

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/provider"
)

type streamReadError struct {
	err error
}

func (e *streamReadError) Error() string { return "read chat completion stream: " + e.err.Error() }
func (e *streamReadError) Unwrap() error { return e.err }

type assembledToolCall struct {
	id        string
	name      string
	arguments string
}

func readStream(body io.Reader, emit func(provider.StreamEvent)) (provider.Response, bool, error) {
	reader := bufio.NewReader(body)
	var dataLines []string
	var text strings.Builder
	var calls []*assembledToolCall
	byIndex := make(map[int]*assembledToolCall)
	var usage model.Usage
	finish := model.FinishUnknown
	emitted := false
	done := false

	dispatch := func() error {
		if len(dataLines) == 0 {
			return nil
		}
		data := strings.Join(dataLines, "\n")
		dataLines = dataLines[:0]
		if data == "[DONE]" {
			done = true
			return nil
		}
		var chunk chatChunk
		if err := json.Unmarshal([]byte(data), &chunk); err != nil {
			return fmt.Errorf("decode chat completion stream: %w", err)
		}
		if chunk.Usage != nil {
			usage = model.Usage{InputTokens: chunk.Usage.PromptTokens, OutputTokens: chunk.Usage.CompletionTokens}
		}
		for _, choice := range chunk.Choices {
			if choice.Delta.Content != "" {
				text.WriteString(choice.Delta.Content)
				event := provider.StreamEvent{Type: provider.StreamTextDelta, Text: choice.Delta.Content}
				emitted = true
				if emit != nil {
					emit(event)
				}
			}
			for _, delta := range choice.Delta.ToolCalls {
				call := byIndex[delta.Index]
				if call == nil {
					call = &assembledToolCall{}
					byIndex[delta.Index] = call
					calls = append(calls, call)
				}
				if delta.ID != "" {
					call.id = delta.ID
				}
				if delta.Function.Name != "" {
					call.name = delta.Function.Name
				}
				call.arguments += delta.Function.Arguments
				event := provider.StreamEvent{
					Type:       provider.StreamToolCallDelta,
					ToolCallID: delta.ID,
					ToolName:   delta.Function.Name,
					Arguments:  delta.Function.Arguments,
				}
				emitted = true
				if emit != nil {
					emit(event)
				}
			}
			if choice.FinishReason != "" {
				finish = finishReason(choice.FinishReason)
			}
		}
		return nil
	}

	for !done {
		line, err := reader.ReadString('\n')
		if len(line) > 0 {
			line = strings.TrimSuffix(line, "\n")
			line = strings.TrimSuffix(line, "\r")
			switch {
			case line == "":
				if dispatchErr := dispatch(); dispatchErr != nil {
					return provider.Response{}, emitted, dispatchErr
				}
			case strings.HasPrefix(line, ":"):
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
				return provider.Response{}, emitted, &streamReadError{err: err}
			}
			if len(dataLines) > 0 {
				if dispatchErr := dispatch(); dispatchErr != nil {
					return provider.Response{}, emitted, dispatchErr
				}
			}
			break
		}
	}
	if !done {
		return provider.Response{}, emitted, fmt.Errorf("chat completion stream ended without [DONE]")
	}

	blocks := make([]model.Block, 0, 1+len(calls))
	if text.Len() > 0 {
		blocks = append(blocks, model.Block{Type: model.BlockText, Text: text.String()})
	}
	for _, call := range calls {
		if !validArguments(call.arguments) {
			return provider.Response{}, emitted, fmt.Errorf("tool call %q has malformed arguments", call.id)
		}
		blocks = append(blocks, model.Block{
			Type:       model.BlockToolCall,
			ToolCallID: call.id,
			ToolName:   call.name,
			Arguments:  json.RawMessage(call.arguments),
		})
	}
	messageUsage := usage
	return provider.Response{
		Message: model.Message{
			Role:         model.RoleAssistant,
			Blocks:       blocks,
			FinishReason: finish,
			Usage:        &messageUsage,
		},
		FinishReason: finish,
		Usage:        usage,
	}, emitted, nil
}
