package agent

import (
	"context"
	"errors"
	"fmt"
	"math"
	"sync/atomic"

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/provider"
	"github.com/baiyuqing/otto/internal/session"
)

// Compact creates a manual context checkpoint. A context with no safe historic
// prefix is a successful no-op.
func (a *Agent) Compact(ctx context.Context, focus string, emit func(Event)) (CompactionResult, error) {
	a.operationMu.Lock()
	defer a.operationMu.Unlock()

	result, err := a.compact(ctx, CompactionManual, focus, emit)
	if errors.Is(err, ErrNothingToCompact) {
		result = CompactionResult{Noop: true}
		a.emitCompaction(emit, EventCompactionCompleted, CompactionResult{Reason: CompactionManual, Noop: true})
		return result, nil
	}
	if err != nil {
		return result, a.fail(emit, err)
	}
	return result, nil
}

// compact owns the provider/session pipeline but not operation locking or
// public error policy. Run can therefore reuse it for automatic compaction.
func (a *Agent) compact(ctx context.Context, reason CompactionReason, focus string, emit func(Event)) (CompactionResult, error) {
	automatic := reason != CompactionManual
	a.emitCompaction(emit, EventCompactionStarted, CompactionResult{Reason: reason, Automatic: automatic})

	if err := ctx.Err(); err != nil {
		return CompactionResult{}, err
	}
	messages := cloneMessages(a.session.Messages())
	latest, hasLatest := a.session.LatestCompaction()
	tools := cloneTools(a.registry.Definitions())
	tokensBefore := estimateRequest(provider.Request{
		Model:        a.options.Model,
		SystemPrompt: a.options.SystemPrompt,
		Thinking:     a.options.Thinking,
		Messages:     messages,
		Tools:        tools,
	}, latest, hasLatest)

	selection, err := selectCompaction(
		messages,
		latest,
		hasLatest,
		a.options.Compaction.KeepRecentTokens,
		compactionRetainedBudget(a.options.Compaction),
	)
	if err != nil {
		if errors.Is(err, ErrNothingToCompact) || errors.Is(err, ErrCurrentTurnTooLarge) {
			return CompactionResult{}, err
		}
		return CompactionResult{}, &compactionBoundaryError{message: "select compaction context failed", cause: err}
	}

	normalizedFocus, err := normalizeCompactionFocus(focus)
	if err != nil {
		return CompactionResult{}, invalidCompactionSummaryError(err)
	}
	preparedSelection := a.redactCompactionSelection(selection)
	preparedLatest := latest
	preparedLatest.Details = a.redactCompactionDetails(latest.Details)
	prepared, turnPrepared, hasTurn, err := a.prepareSummaryRequests(preparedSelection, a.redactor.RedactString(normalizedFocus), preparedLatest, hasLatest)
	if err != nil {
		return CompactionResult{}, invalidCompactionSummaryError(err)
	}

	structured := len(preparedSelection.HistoricalSource) != 0 ||
		len(preparedSelection.TurnPrefixSource) != 0 && preparedSelection.PreviousSummary == ""
	firstMaximumBytes := summaryMaximumBytes
	if !structured {
		firstMaximumBytes = turnSummaryMaximumBytes
	}
	generated, generatedUsage, generatedUsagePresent, err := a.executeSummaryRequest(ctx, prepared.Request, firstMaximumBytes, structured)
	if err != nil {
		return CompactionResult{}, err
	}
	finalSummary := generated
	usage := generatedUsage
	usagePresent := generatedUsagePresent
	if len(preparedSelection.HistoricalSource) == 0 && preparedSelection.PreviousSummary != "" {
		finalSummary, err = combineSummary(preparedSelection.PreviousSummary, generated)
		if err != nil {
			return CompactionResult{}, invalidCompactionSummaryError(err)
		}
	}
	details := prepared.Details

	if hasTurn {
		turn, turnUsage, turnUsagePresent, err := a.executeSummaryRequest(ctx, turnPrepared.Request, turnSummaryMaximumBytes, false)
		if err != nil {
			return CompactionResult{}, err
		}
		finalSummary, err = combineSummary(generated, turn)
		if err != nil {
			return CompactionResult{}, invalidCompactionSummaryError(err)
		}
		usage, usagePresent = combineCompactionUsage(usage, usagePresent, turnUsage, turnUsagePresent)
		details = turnPrepared.Details
	}

	estimatedAfter := estimateCompactedContext(a.options, tools, finalSummary, selection.Retained)
	result := CompactionResult{
		Reason:               reason,
		TokensBefore:         tokensBefore,
		EstimatedTokensAfter: estimatedAfter,
		Automatic:            automatic,
		Usage:                usage,
		UsagePresent:         usagePresent,
	}
	checkpoint := session.CompactionCheckpoint{
		Summary:          finalSummary,
		FirstKeptEntryID: selection.FirstKeptID,
		TokensBefore:     tokensBefore,
		Details:          details,
		CreatedAt:        a.options.Now(),
	}
	if usagePresent {
		checkpointUsage := usage
		checkpoint.Usage = &checkpointUsage
	}

	// This is the final cancelable boundary before handing ownership to the
	// session's durable append transaction.
	if err := ctx.Err(); err != nil {
		return CompactionResult{}, err
	}
	metadata, err := a.session.AppendCompaction(context.WithoutCancel(ctx), checkpoint)
	if err != nil {
		return CompactionResult{}, &compactionBoundaryError{message: "persist compaction checkpoint failed", cause: err}
	}
	result.CheckpointID = metadata.ID
	a.emitCompaction(emit, EventCompactionCompleted, result)

	// A cancellation observed after commit does not erase the committed result
	// or completion event. It still stops an automatic caller before any later
	// provider action.
	if err := ctx.Err(); err != nil {
		return result, err
	}
	return result, nil
}

func (a *Agent) redactCompactionSelection(selection compactionSelection) compactionSelection {
	redacted := selection
	redacted.PreviousSummary = a.redactor.RedactString(redacted.PreviousSummary)
	redactMessages := func(messages []model.Message) []model.Message {
		result := make([]model.Message, len(messages))
		for index, message := range messages {
			result[index] = a.redactMessage(message)
		}
		return result
	}
	redacted.HistoricalSource = redactMessages(selection.HistoricalSource)
	redacted.TurnPrefixSource = redactMessages(selection.TurnPrefixSource)
	redacted.Retained = cloneMessages(selection.Retained)
	return redacted
}

func (a *Agent) redactCompactionDetails(details session.CompactionDetails) session.CompactionDetails {
	redacted := details
	redacted.ReadFiles = append([]string(nil), details.ReadFiles...)
	for index := range redacted.ReadFiles {
		redacted.ReadFiles[index] = a.redactor.RedactString(redacted.ReadFiles[index])
	}
	redacted.ModifiedFiles = append([]string(nil), details.ModifiedFiles...)
	for index := range redacted.ModifiedFiles {
		redacted.ModifiedFiles[index] = a.redactor.RedactString(redacted.ModifiedFiles[index])
	}
	return redacted
}

func (a *Agent) prepareSummaryRequests(
	selection compactionSelection,
	focus string,
	latest session.CompactionMetadata,
	hasLatest bool,
) (summaryRequest, summaryRequest, bool, error) {
	previousDetails := session.CompactionDetails{}
	if hasLatest {
		previousDetails = latest.Details
	}
	requestSelection := selection
	if len(selection.HistoricalSource) == 0 && len(selection.TurnPrefixSource) != 0 && selection.PreviousSummary == "" {
		requestSelection = compactionSelection{HistoricalSource: cloneMessages(selection.TurnPrefixSource)}
	}
	prepared, err := buildSummaryRequest(a.options, requestSelection, focus, previousDetails)
	if err != nil {
		return summaryRequest{}, summaryRequest{}, false, err
	}
	if !selection.SplitTurn || len(selection.HistoricalSource) == 0 || len(selection.TurnPrefixSource) == 0 {
		return prepared, summaryRequest{}, false, nil
	}
	turnSelection := compactionSelection{TurnPrefixSource: cloneMessages(selection.TurnPrefixSource)}
	turnPrepared, err := buildSummaryRequest(a.options, turnSelection, focus, prepared.Details)
	if err != nil {
		return summaryRequest{}, summaryRequest{}, false, err
	}
	return prepared, turnPrepared, true, nil
}

func (a *Agent) executeSummaryRequest(
	ctx context.Context,
	request provider.Request,
	maximumBytes int,
	structured bool,
) (string, model.Usage, bool, error) {
	if err := ctx.Err(); err != nil {
		return "", model.Usage{}, false, err
	}
	childCtx, cancel := context.WithCancel(ctx)
	defer cancel()

	var streamedBytes atomic.Int64
	var invalidStream atomic.Bool
	response, err := a.provider.Complete(childCtx, request, func(event provider.StreamEvent) {
		if invalidStream.Load() {
			return
		}
		if event.Type != provider.StreamTextDelta {
			invalidStream.Store(true)
			cancel()
			return
		}
		deltaBytes := int64(len(event.Text))
		current := streamedBytes.Add(deltaBytes)
		if deltaBytes < 0 || current < 0 || current > int64(maximumBytes) {
			invalidStream.Store(true)
			cancel()
		}
	})
	if ctxErr := ctx.Err(); ctxErr != nil {
		return "", model.Usage{}, false, ctxErr
	}
	if invalidStream.Load() {
		return "", model.Usage{}, false, invalidCompactionSummaryError(errors.New("streamed response exceeded its bound or attempted a tool call"))
	}
	if err != nil {
		return "", model.Usage{}, false, &compactionBoundaryError{message: "compaction summary provider request failed", cause: err}
	}

	message := response.Message
	if message.FinishReason != "" && response.FinishReason != "" && message.FinishReason != response.FinishReason {
		return "", model.Usage{}, false, invalidCompactionSummaryError(errors.New("response finish reasons contradict"))
	}
	if message.FinishReason == "" {
		message.FinishReason = response.FinishReason
	}
	if err := validateRawSummaryResponseBound(message, maximumBytes); err != nil {
		return "", model.Usage{}, false, invalidCompactionSummaryError(err)
	}
	message = a.redactMessage(message)
	if message.Role != model.RoleAssistant {
		return "", model.Usage{}, false, invalidCompactionSummaryError(errors.New("response role is not assistant"))
	}
	var summary string
	if structured {
		summary, err = validateStructuredSummary(message)
	} else {
		summary, err = validateTurnSummary(message)
	}
	if err != nil {
		return "", model.Usage{}, false, invalidCompactionSummaryError(err)
	}

	usagePresent := compactionUsagePresent(response.Usage)
	if usagePresent {
		if err := response.Usage.Validate(); err != nil {
			return "", model.Usage{}, false, invalidCompactionSummaryError(errors.New("response usage is invalid"))
		}
	}
	return summary, response.Usage, usagePresent, nil
}

func validateRawSummaryResponseBound(message model.Message, maximumBytes int) error {
	total := 0
	for _, block := range message.Blocks {
		if block.Type != model.BlockText {
			return errors.New("response contains a non-text block")
		}
		if len(block.Text) > maximumBytes-total {
			return errors.New("response exceeds its byte bound")
		}
		total += len(block.Text)
	}
	return nil
}

type compactionBoundaryError struct {
	message string
	cause   error
}

func (e *compactionBoundaryError) Error() string { return e.message }
func (e *compactionBoundaryError) Unwrap() error { return e.cause }

func invalidCompactionSummaryError(cause error) error {
	if cause == nil {
		return ErrInvalidCompactionSummary
	}
	return fmt.Errorf("%w: %v", ErrInvalidCompactionSummary, cause)
}

func compactionUsagePresent(usage model.Usage) bool {
	return usage.InputTokens != 0 || usage.OutputTokens != 0 || usage.CachedInputTokens != 0
}

func combineCompactionUsage(left model.Usage, leftPresent bool, right model.Usage, rightPresent bool) (model.Usage, bool) {
	if !leftPresent {
		return right, rightPresent
	}
	if !rightPresent {
		return left, true
	}
	return model.Usage{
		InputTokens:       saturatingTokenAdd(left.InputTokens, right.InputTokens),
		OutputTokens:      saturatingTokenAdd(left.OutputTokens, right.OutputTokens),
		CachedInputTokens: saturatingTokenAdd(left.CachedInputTokens, right.CachedInputTokens),
	}, true
}

func saturatingTokenAdd(left, right int) int {
	if left < 0 || right < 0 {
		return 0
	}
	if left > math.MaxInt-right {
		return math.MaxInt
	}
	return left + right
}

func compactionRetainedBudget(settings CompactionSettings) int {
	if settings.HardInputWindow <= 0 {
		return 0
	}
	reserve := max(settings.ReserveTokens, 0)
	if reserve >= settings.HardInputWindow {
		return 1
	}
	return settings.HardInputWindow - reserve
}

func estimateCompactedContext(options Options, tools []model.ToolDefinition, summary string, retained []model.Message) int {
	messages := make([]model.Message, 0, len(retained)+1)
	messages = append(messages, model.Message{
		Role:        model.RoleContext,
		ContextType: "compaction",
		Display:     true,
		Blocks: []model.Block{{
			Type: model.BlockText,
			Text: compactionSummaryDisplayPrefix + summary,
		}},
	})
	messages = append(messages, cloneMessages(retained)...)
	// This request is a synthetic post-checkpoint candidate. An empty active
	// checkpoint floor forces content fallback instead of trusting usage on a
	// retained assistant message from the pre-compaction prompt.
	return estimateRequest(provider.Request{
		Model:        options.Model,
		SystemPrompt: options.SystemPrompt,
		Thinking:     options.Thinking,
		Messages:     messages,
		Tools:        cloneTools(tools),
	}, session.CompactionMetadata{}, true)
}

func (a *Agent) emitCompaction(emit func(Event), eventType EventType, result CompactionResult) {
	compaction := &CompactionEvent{
		CheckpointID:         result.CheckpointID,
		Reason:               result.Reason,
		TokensBefore:         result.TokensBefore,
		EstimatedTokensAfter: result.EstimatedTokensAfter,
		Automatic:            result.Automatic,
		Usage:                result.Usage,
		UsagePresent:         result.UsagePresent,
		Noop:                 result.Noop,
	}
	a.emit(emit, Event{Type: eventType, Compaction: compaction})
}
