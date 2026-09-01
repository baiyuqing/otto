package agent

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
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
	if !redactor.complete {
		options.Model = ""
		options.Thinking = ""
	}
	return &Agent{provider: completionProvider, registry: registry, session: memory, options: options, redactor: redactor}
}

func (a *Agent) Run(ctx context.Context, userText string, emit func(Event)) error {
	a.operationMu.Lock()
	defer a.operationMu.Unlock()

	if text := trimSpace(userText); text == "" {
		return a.fail(emit, ErrEmptyUserText)
	}
	if !a.redactor.complete {
		return a.runWithIncompleteRedactions(ctx, emit)
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

	dispatchState := runDispatchState{}
	for {
		response, err := a.dispatchNormalProviderStep(ctx, emit, &dispatchState)
		if err != nil {
			return a.fail(emit, err)
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

func (a *Agent) dispatchNormalProviderStep(ctx context.Context, emit func(Event), state *runDispatchState) (provider.Response, error) {
	request, estimate := a.buildNormalProviderRequest()
	softTrigger, hardTrigger, knownLimits := automaticCompactionTriggers(a.options.Compaction)

	if a.options.Compaction.Auto && knownLimits && estimate > softTrigger {
		if !state.proactiveAttempted {
			state.proactiveAttempted = true
			if _, err := a.compact(ctx, CompactionThreshold, "", emit); err != nil {
				if errors.Is(err, ErrNothingToCompact) {
					a.emitCompaction(emit, EventCompactionCompleted, CompactionResult{Reason: CompactionThreshold, Automatic: true, Noop: true})
					state.proactiveAttempted = false
				} else {
					if cancelErr := automaticCancellation(ctx, err); cancelErr != nil {
						return provider.Response{}, cancelErr
					}
					if estimate >= hardTrigger {
						return provider.Response{}, newAutomaticDispatchError(automaticCompactionHardFailureMessage, err)
					}
					a.emit(emit, Event{Type: EventCompactionWarning, Err: errAutomaticCompactionWarning})
				}
			} else {
				if err := ctx.Err(); err != nil {
					return provider.Response{}, err
				}
				request, estimate = a.buildNormalProviderRequest()
				if estimate > hardTrigger {
					return provider.Response{}, newAutomaticDispatchError(automaticCompactionStillTooLargeMessage)
				}
			}
		} else if estimate > hardTrigger {
			return provider.Response{}, newAutomaticDispatchError(automaticCompactionAttemptUsedMessage)
		}
	}

	response, visibleText, err := a.completeNormalProviderAttempt(ctx, request, emit)
	if err == nil || !a.options.Compaction.Auto || visibleText || !isTypedContextOverflow(err) {
		return response, err
	}

	originalOverflow := err
	if _, compactionErr := a.compact(ctx, CompactionOverflow, "", emit); compactionErr != nil {
		if errors.Is(compactionErr, ErrNothingToCompact) {
			a.emitCompaction(emit, EventCompactionCompleted, CompactionResult{Reason: CompactionOverflow, Automatic: true, Noop: true})
			return provider.Response{}, originalOverflow
		}
		if cancelErr := automaticCancellation(ctx, compactionErr); cancelErr != nil {
			return provider.Response{}, cancelErr
		}
		return provider.Response{}, newAutomaticDispatchError(overflowCompactionFailureMessage, originalOverflow, compactionErr)
	}
	if err := ctx.Err(); err != nil {
		return provider.Response{}, err
	}

	retryRequest, retryEstimate := a.buildNormalProviderRequest()
	if _, hardTrigger, knownLimits := automaticCompactionTriggers(a.options.Compaction); knownLimits && retryEstimate > hardTrigger {
		return provider.Response{}, newAutomaticDispatchError(automaticCompactionStillTooLargeMessage, originalOverflow)
	}
	response, _, err = a.completeNormalProviderAttempt(ctx, retryRequest, emit)
	if isTypedContextOverflow(err) {
		return provider.Response{}, newAutomaticDispatchError(overflowRetryFailureMessage, originalOverflow, err)
	}
	return response, err
}

func (a *Agent) buildNormalProviderRequest() (provider.Request, int) {
	var messages []model.Message
	if a.redactor.complete {
		messages = cloneMessages(a.session.Messages())
		for index := range messages {
			messages[index] = a.redactMessage(messages[index])
		}
	}
	tools := cloneTools(a.registry.Definitions())
	request := provider.Request{
		Model:        a.options.Model,
		SystemPrompt: a.options.SystemPrompt,
		Thinking:     a.options.Thinking,
		Messages:     messages,
		Tools:        tools,
	}
	latest, hasLatest := a.session.LatestCompaction()
	return request, estimateRequest(request, latest, hasLatest)
}

func (a *Agent) completeNormalProviderAttempt(ctx context.Context, request provider.Request, emit func(Event)) (provider.Response, bool, error) {
	stream := a.redactor.newStream()
	visibleText := false
	response, err := a.provider.Complete(ctx, request, func(event provider.StreamEvent) {
		if event.Type != provider.StreamTextDelta {
			return
		}
		if text := stream.Write(event.Text); text != "" {
			visibleText = true
			a.emit(emit, Event{Type: EventTextDelta, Text: text})
		}
	})
	if err != nil {
		return provider.Response{}, visibleText, err
	}
	if text := stream.Flush(); text != "" {
		visibleText = true
		a.emit(emit, Event{Type: EventTextDelta, Text: text})
	}
	return response, visibleText, nil
}

func (a *Agent) emit(emit func(Event), event Event) {
	if emit != nil {
		emit(event)
	}
}

func (a *Agent) runWithIncompleteRedactions(ctx context.Context, emit func(Event)) error {
	a.emit(emit, Event{Type: EventAgentStarted})
	if err := ctx.Err(); err != nil {
		return a.fail(emit, err)
	}
	response, err := a.dispatchNormalProviderStep(ctx, emit, &runDispatchState{})
	if err != nil {
		return a.fail(emit, err)
	}
	if err := ctx.Err(); err != nil {
		return a.fail(emit, err)
	}
	a.emit(emit, Event{Type: EventProviderUsage, Usage: response.Usage})
	a.emit(emit, Event{Type: EventAgentFinished})
	return nil
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
