package agent

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"sync"
	"time"

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/provider"
	"github.com/baiyuqing/otto/internal/session"
	"github.com/baiyuqing/otto/internal/tool"
)

type Agent struct {
	operationMu sync.Mutex
	provider    provider.Provider
	registry    *tool.Registry
	session     session.Session
	options     Options
	redactor    *Redactor
}

func New(completionProvider provider.Provider, registry *tool.Registry, memory session.Session, options Options, redactors ...*Redactor) *Agent {
	if registry == nil {
		registry, _ = tool.NewRegistry()
	}
	if options.Now == nil {
		options.Now = func() time.Time { return time.Now().UTC() }
	}
	if options.NewID == nil {
		options.NewID = defaultNewID
	}
	if options.RequestSizer == nil {
		if requestSizer, ok := completionProvider.(provider.RequestSizer); ok {
			options.RequestSizer = requestSizer
		}
	}
	var redactor *Redactor
	if len(redactors) > 0 {
		redactor = redactors[0]
	}
	if redactor == nil {
		redactor = NewRedactor(nil)
	}
	return &Agent{provider: completionProvider, registry: registry, session: memory, options: options, redactor: redactor}
}

func (a *Agent) Run(ctx context.Context, userText string, emit func(Event)) error {
	a.operationMu.Lock()
	defer a.operationMu.Unlock()

	if text := trimSpace(userText); text == "" {
		return a.fail(emit, ErrEmptyUserText)
	}

	a.emit(emit, Event{Type: EventAgentStarted})
	userText = a.redactor.RedactString(userText)
	if err := a.session.Append(ctx, model.Message{
		ID:        a.options.NewID(),
		Role:      model.RoleUser,
		CreatedAt: a.options.Now(),
		Blocks:    []model.Block{{Type: model.BlockText, Text: userText}},
	}); err != nil {
		return a.fail(emit, fmt.Errorf("persist user message: %w", err))
	}

	for {
		stream := a.redactor.newStream()
		response, err := a.provider.Complete(ctx, provider.Request{
			Model:        a.options.Model,
			SystemPrompt: a.options.SystemPrompt,
			Thinking:     a.options.Thinking,
			Messages:     cloneMessages(a.session.Messages()),
			Tools:        cloneTools(a.registry.Definitions()),
		}, func(event provider.StreamEvent) {
			switch event.Type {
			case provider.StreamTextDelta:
				if text := stream.Write(event.Text); text != "" {
					a.emit(emit, Event{Type: EventTextDelta, Text: text})
				}
			}
		})
		if err != nil {
			return a.fail(emit, err)
		}
		if text := stream.Flush(); text != "" {
			a.emit(emit, Event{Type: EventTextDelta, Text: text})
		}

		assistant := a.redactMessage(response.Message)
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
			result.Content = a.redactor.RedactString(result.Content)
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
}

func (a *Agent) emit(emit func(Event), event Event) {
	if emit != nil {
		emit(event)
	}
}

func (a *Agent) fail(emit func(Event), err error) error {
	err = a.redactor.RedactError(err)
	a.emit(emit, Event{Type: EventAgentError, Err: err})
	return err
}

func (a *Agent) redactMessage(message model.Message) model.Message {
	redacted := cloneMessage(message)
	redacted.ID = a.redactor.RedactString(redacted.ID)
	for index := range redacted.Blocks {
		block := &redacted.Blocks[index]
		block.Text = a.redactor.RedactString(block.Text)
		block.ToolCallID = a.redactor.RedactString(block.ToolCallID)
		block.ToolName = a.redactor.RedactString(block.ToolName)
		if block.Type == model.BlockToolCall {
			block.Arguments = a.redactor.RedactJSONStrings(block.Arguments)
		}
	}
	return redacted
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
