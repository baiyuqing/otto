package agent

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"time"

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/provider"
	"github.com/baiyuqing/otto/internal/session"
	"github.com/baiyuqing/otto/internal/tool"
)

type Agent struct {
	provider provider.Provider
	registry *tool.Registry
	session  session.Session
	options  Options
}

func New(provider provider.Provider, registry *tool.Registry, memory session.Session, options Options) *Agent {
	if registry == nil {
		registry, _ = tool.NewRegistry()
	}
	if options.Now == nil {
		options.Now = func() time.Time { return time.Now().UTC() }
	}
	if options.NewID == nil {
		options.NewID = defaultNewID
	}
	if options.MaxTurns <= 0 {
		options.MaxTurns = 20
	}
	return &Agent{provider: provider, registry: registry, session: memory, options: options}
}

func (a *Agent) Run(ctx context.Context, userText string, emit func(Event)) error {
	if text := trimSpace(userText); text == "" {
		return a.fail(emit, ErrEmptyUserText)
	}

	a.emit(emit, Event{Type: EventAgentStarted})
	if err := a.session.Append(ctx, model.Message{
		ID:        a.options.NewID(),
		Role:      model.RoleUser,
		CreatedAt: a.options.Now(),
		Blocks:    []model.Block{{Type: model.BlockText, Text: userText}},
	}); err != nil {
		return a.fail(emit, fmt.Errorf("persist user message: %w", err))
	}

	for turn := 0; turn < a.options.MaxTurns; turn++ {
		response, err := a.provider.Complete(ctx, provider.Request{
			Model:        a.options.Model,
			SystemPrompt: a.options.SystemPrompt,
			Messages:     cloneMessages(a.session.Messages()),
			Tools:        cloneTools(a.registry.Definitions()),
		}, func(event provider.StreamEvent) {
			switch event.Type {
			case provider.StreamTextDelta:
				a.emit(emit, Event{Type: EventTextDelta, Text: event.Text})
			}
		})
		if err != nil {
			return a.fail(emit, err)
		}

		assistant := cloneMessage(response.Message)
		if assistant.ID == "" {
			assistant.ID = a.options.NewID()
		}
		if assistant.Role == "" {
			assistant.Role = model.RoleAssistant
		}
		if assistant.CreatedAt.IsZero() {
			assistant.CreatedAt = a.options.Now()
		}
		assistant.FinishReason = response.FinishReason
		if response.Usage.InputTokens != 0 || response.Usage.OutputTokens != 0 {
			usage := response.Usage
			assistant.Usage = &usage
		} else {
			assistant.Usage = nil
		}

		if err := a.session.Append(ctx, assistant); err != nil {
			return a.fail(emit, fmt.Errorf("persist assistant message: %w", err))
		}
		a.emit(emit, Event{Type: EventProviderUsage, Usage: response.Usage})

		durabilityCtx := context.WithoutCancel(ctx)
		hadToolCall := false
		// Tool calls are executed sequentially. At most one tool is active at a
		// time, which lets frontends reserve bounded delivery for its terminal event.
		for _, block := range assistant.Blocks {
			if block.Type != model.BlockToolCall {
				continue
			}
			hadToolCall = true
			a.emit(emit, Event{Type: EventToolCallStarted, ToolName: block.ToolName, ToolCallID: block.ToolCallID, ToolArgs: string(block.Arguments)})
			var result tool.Result
			if err := ctx.Err(); err != nil {
				result = tool.Result{Content: err.Error(), IsError: true}
			} else {
				result = a.registry.Execute(ctx, block.ToolName, cloneArguments(block.Arguments))
			}
			a.emit(emit, Event{Type: EventToolCallFinished, ToolName: block.ToolName, ToolCallID: block.ToolCallID, ToolResult: result})
			if err := a.session.Append(durabilityCtx, model.Message{
				ID:        a.options.NewID(),
				Role:      model.RoleTool,
				CreatedAt: a.options.Now(),
				Blocks: []model.Block{{
					Type:       model.BlockToolResult,
					Text:       result.Content,
					ToolCallID: block.ToolCallID,
					ToolName:   block.ToolName,
					IsError:    result.IsError,
				}},
			}); err != nil {
				return a.fail(emit, fmt.Errorf("persist tool result for %q: %w", block.ToolCallID, err))
			}
		}
		if err := ctx.Err(); err != nil {
			return a.fail(emit, err)
		}
		if !hadToolCall {
			a.emit(emit, Event{Type: EventAgentFinished})
			return nil
		}
	}

	return a.fail(emit, ErrMaxTurns)
}

func (a *Agent) emit(emit func(Event), event Event) {
	if emit != nil {
		emit(event)
	}
}

func (a *Agent) fail(emit func(Event), err error) error {
	a.emit(emit, Event{Type: EventAgentError, Err: err})
	return err
}

func defaultNewID() string {
	buf := make([]byte, 16)
	if _, err := rand.Read(buf); err != nil {
		panic(err)
	}
	return hex.EncodeToString(buf)
}

func cloneTools(tools []model.ToolDefinition) []model.ToolDefinition {
	if tools == nil {
		return nil
	}
	cloned := make([]model.ToolDefinition, len(tools))
	copy(cloned, tools)
	return cloned
}

func cloneMessages(messages []model.Message) []model.Message {
	if messages == nil {
		return nil
	}
	cloned := make([]model.Message, len(messages))
	for i, message := range messages {
		cloned[i] = cloneMessage(message)
	}
	return cloned
}

func cloneMessage(message model.Message) model.Message {
	cloned := message
	if message.Blocks != nil {
		cloned.Blocks = make([]model.Block, len(message.Blocks))
		for i, block := range message.Blocks {
			cloned.Blocks[i] = block
			cloned.Blocks[i].Arguments = cloneArguments(block.Arguments)
		}
	}
	if message.Usage != nil {
		usage := *message.Usage
		cloned.Usage = &usage
	}
	return cloned
}

func cloneArguments(arguments json.RawMessage) json.RawMessage {
	if arguments == nil {
		return nil
	}
	return append(json.RawMessage(nil), arguments...)
}

func trimSpace(text string) string {
	start := 0
	for start < len(text) {
		switch text[start] {
		case ' ', '\t', '\n', '\r':
			start++
		default:
			goto trimEnd
		}
	}
	return ""

trimEnd:
	end := len(text)
	for end > start {
		switch text[end-1] {
		case ' ', '\t', '\n', '\r':
			end--
		default:
			return text[start:end]
		}
	}
	return ""
}
