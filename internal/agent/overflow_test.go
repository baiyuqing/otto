package agent

import (
	"context"
	"encoding/json"
	"errors"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/provider"
	"github.com/baiyuqing/otto/internal/session"
	"github.com/baiyuqing/otto/internal/tool"
)

const automaticNormalSystemPrompt = "normal automatic system"

func TestRunAutomaticThresholdBelowEqualAndAboveSoftTrigger(t *testing.T) {
	for _, test := range []struct {
		name       string
		softOffset int
		compacts   bool
	}{
		{name: "below", softOffset: 1},
		{name: "equal", softOffset: 0},
		{name: "above", softOffset: -1, compacts: true},
	} {
		t.Run(test.name, func(t *testing.T) {
			memory := automaticMemory(t)
			settings := automaticSettings()
			options := automaticOptions(settings)
			estimate := automaticRequestEstimate(memory, options, nil, "continue")
			softTrigger := estimate + test.softOffset
			options.Compaction.WorkingWindow = softTrigger + options.Compaction.ReserveTokens
			options.Compaction.HardInputWindow = estimate + 2_000 + options.Compaction.ReserveTokens

			steps := []automaticProviderStep{{response: automaticTextResponse("done", model.FinishStop)}}
			if test.compacts {
				steps = append([]automaticProviderStep{{response: validCompactionResponse(model.Usage{})}}, steps...)
			}
			fake := &automaticProvider{steps: steps}
			runner := New(fake, nil, memory, options)
			var events []Event
			if err := runner.Run(context.Background(), "continue", func(event Event) { events = append(events, event) }); err != nil {
				t.Fatal(err)
			}

			wantCalls := 1
			if test.compacts {
				wantCalls = 2
			}
			if len(fake.requests) != wantCalls {
				t.Fatalf("provider calls = %d, want %d", len(fake.requests), wantCalls)
			}
			if got := countAutomaticEvents(events, EventCompactionCompleted); got != boolInt(test.compacts) {
				t.Fatalf("compaction completions = %d, want %d; events=%#v", got, boolInt(test.compacts), events)
			}
			if test.compacts {
				if fake.requests[0].SystemPrompt == automaticNormalSystemPrompt || fake.requests[1].SystemPrompt != automaticNormalSystemPrompt {
					t.Fatalf("request order is not summary then normal: %#v", fake.requests)
				}
				completion := firstAutomaticEvent(events, EventCompactionCompleted)
				if completion.Compaction == nil || completion.Compaction.Reason != CompactionThreshold {
					t.Fatalf("completion = %#v", completion)
				}
			}
		})
	}
}

func TestRunAutomaticThresholdIncludesToolDefinitions(t *testing.T) {
	registry, err := tool.NewRegistry(largeAutomaticDefinitionTool{})
	if err != nil {
		t.Fatal(err)
	}
	memory := automaticMemory(t)
	options := automaticOptions(automaticSettings())
	withTools := automaticRequestEstimate(memory, options, registry, "continue")
	withoutTools := automaticRequestEstimate(memory, options, nil, "continue")
	if withTools <= withoutTools {
		t.Fatalf("estimates with/without tools = %d/%d", withTools, withoutTools)
	}
	options.Compaction.WorkingWindow = withTools - 1 + options.Compaction.ReserveTokens
	options.Compaction.HardInputWindow = withTools + 20_000 + options.Compaction.ReserveTokens
	fake := &automaticProvider{steps: []automaticProviderStep{
		{response: validCompactionResponse(model.Usage{})},
		{response: automaticTextResponse("done", model.FinishStop)},
	}}
	runner := New(fake, registry, memory, options)

	if err := runner.Run(context.Background(), "continue", nil); err != nil {
		t.Fatal(err)
	}
	if len(fake.requests) != 2 || fake.requests[0].SystemPrompt == automaticNormalSystemPrompt || fake.requests[1].SystemPrompt != automaticNormalSystemPrompt {
		t.Fatalf("requests = %#v, want summary then normal", fake.requests)
	}
}

func TestRunAutomaticHardThresholdEqualityStopsAfterCompactionFailure(t *testing.T) {
	memory := automaticMemory(t)
	options := automaticOptions(automaticSettings())
	options.SystemPrompt = strings.Repeat("normal-system-pressure ", 2_000)
	estimate := automaticRequestEstimate(memory, options, nil, "continue")
	options.Compaction.WorkingWindow = estimate - 1 + options.Compaction.ReserveTokens
	options.Compaction.HardInputWindow = estimate + options.Compaction.ReserveTokens
	secret := strings.Repeat("unbounded-summary-provider-body", 2_000)
	fake := &automaticProvider{steps: []automaticProviderStep{{err: errors.New(secret)}}}
	runner := New(fake, nil, memory, options)

	var events []Event
	err := runner.Run(context.Background(), "continue", func(event Event) { events = append(events, event) })
	if err == nil {
		t.Fatal("Run() error = nil, want hard-boundary failure")
	}
	if len(fake.requests) != 1 || fake.requests[0].SystemPrompt == automaticNormalSystemPrompt {
		t.Fatalf("requests = %#v, want one summary request and no normal request", fake.requests)
	}
	if strings.Contains(err.Error(), secret) || len(err.Error()) > 512 {
		t.Fatalf("hard failure is unbounded or leaked provider text: len=%d err=%q", len(err.Error()), err)
	}
	if got := countAutomaticEvents(events, EventCompactionWarning); got != 0 {
		t.Fatalf("warnings = %d, want 0", got)
	}
}

func TestRunAutomaticThresholdPreflightRunsAfterToolResult(t *testing.T) {
	largeResult := strings.Repeat("tool-result-", 1_500)
	recorder := &recordingTool{name: "large"}
	recorder.execute = func(context.Context, json.RawMessage) tool.Result { return tool.Result{Content: largeResult} }
	registry, err := tool.NewRegistry(recorder)
	if err != nil {
		t.Fatal(err)
	}
	memory := automaticMemory(t)
	options := automaticOptions(automaticSettings())
	firstEstimate := automaticRequestEstimate(memory, options, registry, "continue")
	options.Compaction.WorkingWindow = firstEstimate + 100 + options.Compaction.ReserveTokens
	options.Compaction.HardInputWindow = firstEstimate + 20_000 + options.Compaction.ReserveTokens
	fake := &automaticProvider{steps: []automaticProviderStep{
		{response: automaticToolResponse("call-large", "large")},
		{response: validCompactionResponse(model.Usage{})},
		{response: automaticTextResponse("done", model.FinishStop)},
	}}
	runner := New(fake, registry, memory, options)

	var events []Event
	if err := runner.Run(context.Background(), "continue", func(event Event) { events = append(events, event) }); err != nil {
		t.Fatal(err)
	}
	if len(fake.requests) != 3 || fake.requests[0].SystemPrompt != automaticNormalSystemPrompt ||
		fake.requests[1].SystemPrompt == automaticNormalSystemPrompt || fake.requests[2].SystemPrompt != automaticNormalSystemPrompt {
		t.Fatalf("requests are not normal, summary, normal: %#v", fake.requests)
	}
	if got := countAutomaticEvents(events, EventCompactionCompleted); got != 1 {
		t.Fatalf("compaction completions = %d, want 1; events=%#v", got, events)
	}
}

func TestRunAutomaticThresholdAttemptsProactiveCompactionOnlyOncePerRun(t *testing.T) {
	memory := automaticLargeMemory(t)
	largeResult := strings.Repeat("later-tool-result-", 2_000)
	recorder := &recordingTool{name: "large"}
	recorder.execute = func(context.Context, json.RawMessage) tool.Result { return tool.Result{Content: largeResult} }
	registry, err := tool.NewRegistry(recorder)
	if err != nil {
		t.Fatal(err)
	}
	options := automaticOptions(automaticSettings())
	estimate := automaticRequestEstimate(memory, options, registry, "continue")
	options.Compaction.WorkingWindow = estimate - 1 + options.Compaction.ReserveTokens
	options.Compaction.HardInputWindow = 100_000
	fake := &automaticProvider{steps: []automaticProviderStep{
		{response: validCompactionResponse(model.Usage{})},
		{response: automaticToolResponse("call-large", "large")},
		{response: automaticTextResponse("done", model.FinishStop)},
	}}
	runner := New(fake, registry, memory, options)

	var events []Event
	if err := runner.Run(context.Background(), "continue", func(event Event) { events = append(events, event) }); err != nil {
		t.Fatal(err)
	}
	if len(fake.requests) != 3 {
		t.Fatalf("provider calls = %d, want summary plus two normal calls", len(fake.requests))
	}
	if got := countAutomaticEvents(events, EventCompactionStarted); got != 1 {
		t.Fatalf("compaction attempts = %d, want 1", got)
	}
}

func TestRunAutomaticThresholdSoftFailureWarnsAndSendsOriginalRequest(t *testing.T) {
	memory := automaticMemory(t)
	options := automaticOptions(automaticSettings())
	estimate := automaticRequestEstimate(memory, options, nil, "continue")
	options.Compaction.WorkingWindow = estimate - 1 + options.Compaction.ReserveTokens
	options.Compaction.HardInputWindow = estimate + 1_000 + options.Compaction.ReserveTokens
	secret := "summary-provider-secret-" + strings.Repeat("x", 4_096)
	fake := &automaticProvider{steps: []automaticProviderStep{
		{err: errors.New(secret)},
		{response: automaticTextResponse("done", model.FinishStop)},
	}}
	runner := New(fake, nil, memory, options)

	var events []Event
	if err := runner.Run(context.Background(), "continue", func(event Event) { events = append(events, event) }); err != nil {
		t.Fatal(err)
	}
	if len(fake.requests) != 2 || fake.requests[0].SystemPrompt == automaticNormalSystemPrompt || fake.requests[1].SystemPrompt != automaticNormalSystemPrompt {
		t.Fatalf("requests = %#v, want failed summary then original normal request", fake.requests)
	}
	warning := firstAutomaticEvent(events, EventCompactionWarning)
	if warning.Type != EventCompactionWarning || warning.Err == nil {
		t.Fatalf("warning = %#v", warning)
	}
	if strings.Contains(warning.Err.Error(), secret) || len(warning.Err.Error()) > 256 {
		t.Fatalf("warning is unbounded or leaked provider text: len=%d warning=%q", len(warning.Err.Error()), warning.Err)
	}
	if countAutomaticEvents(events, EventCompactionWarning) != 1 || countAutomaticEvents(events, EventCompactionCompleted) != 0 {
		t.Fatalf("events = %#v", events)
	}
}

func TestRunAutomaticThresholdCancellationDuringSoftFailureStopsOriginalRequest(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	memory := automaticMemory(t)
	options := automaticOptions(automaticSettings())
	estimate := automaticRequestEstimate(memory, options, nil, "continue")
	options.Compaction.WorkingWindow = estimate - 1 + options.Compaction.ReserveTokens
	options.Compaction.HardInputWindow = estimate + 1_000 + options.Compaction.ReserveTokens
	fake := &automaticProvider{steps: []automaticProviderStep{{
		before: func(context.Context) { cancel() },
		err:    errors.New("summary failed after cancellation"),
	}}}
	runner := New(fake, nil, memory, options)

	var events []Event
	err := runner.Run(ctx, "continue", func(event Event) { events = append(events, event) })
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("Run() error = %v, want context.Canceled", err)
	}
	if len(fake.requests) != 1 || fake.requests[0].SystemPrompt == automaticNormalSystemPrompt {
		t.Fatalf("requests = %#v, want only summary request", fake.requests)
	}
	if countAutomaticEvents(events, EventCompactionWarning) != 0 {
		t.Fatalf("events = %#v, want no warning", events)
	}
}

func TestRunAutomaticThresholdSuccessfulCompactionStillAtHardLimitStopsDispatch(t *testing.T) {
	memory := automaticMemory(t)
	settings := automaticSettings()
	options := automaticOptions(settings)
	options.SystemPrompt = strings.Repeat("system-pressure-", 300)
	hardTrigger := estimateString(options.SystemPrompt) + 40
	options.Compaction.WorkingWindow = hardTrigger - 1 + options.Compaction.ReserveTokens
	options.Compaction.HardInputWindow = hardTrigger + options.Compaction.ReserveTokens
	fake := &automaticProvider{steps: []automaticProviderStep{{response: validCompactionResponse(model.Usage{})}}}
	runner := New(fake, nil, memory, options)

	err := runner.Run(context.Background(), "continue", nil)
	if err == nil {
		t.Fatal("Run() error = nil, want rebuilt-context hard failure")
	}
	if len(fake.requests) != 1 || fake.requests[0].SystemPrompt == options.SystemPrompt {
		t.Fatalf("requests = %#v, want successful summary and no normal dispatch", fake.requests)
	}
	if _, ok := memory.LatestCompaction(); !ok {
		t.Fatal("successful compaction checkpoint was not committed")
	}
	if len(err.Error()) > 512 {
		t.Fatalf("error length = %d, want bounded", len(err.Error()))
	}
}

func TestRunAutomaticUnknownModelNeverCompactsProactively(t *testing.T) {
	memory := automaticLargeMemory(t)
	settings := automaticSettings()
	settings.WorkingWindow = 0
	settings.HardInputWindow = 0
	fake := &automaticProvider{steps: []automaticProviderStep{{response: automaticTextResponse("done", model.FinishStop)}}}
	runner := New(fake, nil, memory, automaticOptions(settings))

	var events []Event
	if err := runner.Run(context.Background(), "continue", func(event Event) { events = append(events, event) }); err != nil {
		t.Fatal(err)
	}
	if len(fake.requests) != 1 || fake.requests[0].SystemPrompt != automaticNormalSystemPrompt {
		t.Fatalf("requests = %#v, want one normal request", fake.requests)
	}
	if countAutomaticEvents(events, EventCompactionStarted) != 0 {
		t.Fatalf("events = %#v, want no proactive compaction", events)
	}
}

func TestRunAutomaticUnknownModelRecoversTypedOverflow(t *testing.T) {
	settings := automaticSettings()
	settings.WorkingWindow = 0
	settings.HardInputWindow = 0
	fake := &automaticProvider{steps: []automaticProviderStep{
		{err: automaticOverflow()},
		{response: validCompactionResponse(model.Usage{InputTokens: 50, OutputTokens: 10})},
		{response: automaticTextResponse("done", model.FinishStop)},
	}}
	memory := automaticMemory(t)
	runner := New(fake, nil, memory, automaticOptions(settings))

	var events []Event
	if err := runner.Run(context.Background(), "continue", func(event Event) { events = append(events, event) }); err != nil {
		t.Fatal(err)
	}
	if len(fake.requests) != 3 {
		t.Fatalf("provider calls = %d, want overflow, summary, retry", len(fake.requests))
	}
	if fake.requests[0].SystemPrompt != automaticNormalSystemPrompt || fake.requests[1].SystemPrompt == automaticNormalSystemPrompt ||
		fake.requests[2].SystemPrompt != automaticNormalSystemPrompt || len(fake.requests[2].Messages) == 0 ||
		fake.requests[2].Messages[0].Role != model.RoleContext || fake.requests[2].Messages[0].ContextType != "compaction" {
		t.Fatalf("retry did not rebuild compacted normal request: %#v", fake.requests)
	}
	completion := firstAutomaticEvent(events, EventCompactionCompleted)
	if completion.Compaction == nil || completion.Compaction.Reason != CompactionOverflow || !completion.Compaction.Automatic {
		t.Fatalf("completion = %#v", completion)
	}
}

func TestRunOverflowRecoveryCompactionFailureIsBoundedAndPreservesOverflow(t *testing.T) {
	secret := strings.Repeat("unsummarized-session-secret", 2_000)
	memory := session.NewMemory(testHeader(t))
	fake := &automaticProvider{steps: []automaticProviderStep{{err: automaticOverflow()}}}
	runner := New(fake, nil, memory, automaticOptions(automaticUnknownSettings()))

	err := runner.Run(context.Background(), secret, nil)
	if !errors.Is(err, provider.ErrContextOverflow) {
		t.Fatalf("Run() error = %v, want context overflow identity", err)
	}
	if strings.Contains(err.Error(), secret) || len(err.Error()) > 512 {
		t.Fatalf("recovery failure is unbounded or leaked context: len=%d err=%q", len(err.Error()), err)
	}
	if len(fake.requests) != 1 {
		t.Fatalf("provider calls = %d, want failed normal call and no summary call", len(fake.requests))
	}
}

func TestRunOverflowCancellationAfterCommittedCompactionStopsRetry(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	memory := automaticMemory(t)
	wrapped := &compactionHookSession{Session: memory, afterAppend: cancel}
	fake := &automaticProvider{steps: []automaticProviderStep{
		{err: automaticOverflow()},
		{response: validCompactionResponse(model.Usage{})},
	}}
	runner := New(fake, nil, wrapped, automaticOptions(automaticUnknownSettings()))

	var events []Event
	err := runner.Run(ctx, "continue", func(event Event) { events = append(events, event) })
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("Run() error = %v, want context.Canceled", err)
	}
	if len(fake.requests) != 2 {
		t.Fatalf("provider calls = %d, want overflow plus summary and no retry", len(fake.requests))
	}
	if _, ok := memory.LatestCompaction(); !ok || countAutomaticEvents(events, EventCompactionCompleted) != 1 {
		t.Fatalf("committed checkpoint/completion missing: events=%#v", events)
	}
}

func TestRunOverflowAfterVisibleRedactedTextDoesNotRetry(t *testing.T) {
	fake := &automaticProvider{steps: []automaticProviderStep{{
		stream: []provider.StreamEvent{{Type: provider.StreamTextDelta, Text: "already visible"}},
		err:    automaticOverflow(),
	}}}
	memory := automaticMemory(t)
	runner := New(fake, nil, memory, automaticOptions(automaticUnknownSettings()))

	var events []Event
	err := runner.Run(context.Background(), "continue", func(event Event) { events = append(events, event) })
	if !errors.Is(err, provider.ErrContextOverflow) {
		t.Fatalf("Run() error = %v, want context overflow", err)
	}
	if len(fake.requests) != 1 || countAutomaticEvents(events, EventCompactionStarted) != 0 {
		t.Fatalf("calls=%d events=%#v, want no recovery", len(fake.requests), events)
	}
	if text := automaticEventText(events); text != "already visible" {
		t.Fatalf("visible text = %q", text)
	}
}

func TestRunOverflowAfterOnlyBufferedRedactorInputCanRetry(t *testing.T) {
	fake := &automaticProvider{steps: []automaticProviderStep{
		{stream: []provider.StreamEvent{{Type: provider.StreamTextDelta, Text: "sec"}}, err: automaticOverflow()},
		{response: validCompactionResponse(model.Usage{})},
		{response: automaticTextResponse("done", model.FinishStop)},
	}}
	memory := automaticMemory(t)
	runner := New(fake, nil, memory, automaticOptions(automaticUnknownSettings()), NewRedactor([]string{"secret"}))

	var events []Event
	if err := runner.Run(context.Background(), "continue", func(event Event) { events = append(events, event) }); err != nil {
		t.Fatal(err)
	}
	if len(fake.requests) != 3 {
		t.Fatalf("provider calls = %d, want recovery after withheld output", len(fake.requests))
	}
	if text := automaticEventText(events); text != "done" {
		t.Fatalf("visible text = %q, want only retry output", text)
	}
}

func TestRunOverflowAfterOnlyToolCallDeltaCanRetry(t *testing.T) {
	fake := &automaticProvider{steps: []automaticProviderStep{
		{stream: []provider.StreamEvent{{Type: provider.StreamToolCallDelta, ToolCallID: "partial", ToolName: "read"}}, err: automaticOverflow()},
		{response: validCompactionResponse(model.Usage{})},
		{response: automaticTextResponse("done", model.FinishStop)},
	}}
	memory := automaticMemory(t)
	runner := New(fake, nil, memory, automaticOptions(automaticUnknownSettings()))

	if err := runner.Run(context.Background(), "continue", nil); err != nil {
		t.Fatal(err)
	}
	if len(fake.requests) != 3 {
		t.Fatalf("provider calls = %d, want overflow, summary, retry", len(fake.requests))
	}
}

func TestRunOverflowRetryOverflowsAgainAndStops(t *testing.T) {
	fake := &automaticProvider{steps: []automaticProviderStep{
		{err: automaticOverflow()},
		{response: validCompactionResponse(model.Usage{})},
		{err: automaticOverflow()},
	}}
	memory := automaticMemory(t)
	runner := New(fake, nil, memory, automaticOptions(automaticUnknownSettings()))

	err := runner.Run(context.Background(), "continue", nil)
	if !errors.Is(err, provider.ErrContextOverflow) {
		t.Fatalf("Run() error = %v, want context overflow", err)
	}
	if len(fake.requests) != 3 {
		t.Fatalf("provider calls = %d, want exactly one summary and one retry", len(fake.requests))
	}
}

func TestRunAutomaticProactiveFailureCanStillRecoverReactiveOverflow(t *testing.T) {
	memory := automaticMemory(t)
	options := automaticOptions(automaticSettings())
	estimate := automaticRequestEstimate(memory, options, nil, "continue")
	options.Compaction.WorkingWindow = estimate - 1 + options.Compaction.ReserveTokens
	options.Compaction.HardInputWindow = estimate + 1_000 + options.Compaction.ReserveTokens
	fake := &automaticProvider{steps: []automaticProviderStep{
		{err: errors.New("optional proactive summary failed")},
		{err: automaticOverflow()},
		{response: validCompactionResponse(model.Usage{})},
		{response: automaticTextResponse("done", model.FinishStop)},
	}}
	runner := New(fake, nil, memory, options)

	var events []Event
	if err := runner.Run(context.Background(), "continue", func(event Event) { events = append(events, event) }); err != nil {
		t.Fatal(err)
	}
	if len(fake.requests) != 4 {
		t.Fatalf("provider calls = %d, want proactive, original, reactive, retry", len(fake.requests))
	}
	if countAutomaticEvents(events, EventCompactionWarning) != 1 || countAutomaticEvents(events, EventCompactionStarted) != 2 || countAutomaticEvents(events, EventCompactionCompleted) != 1 {
		t.Fatalf("events = %#v", events)
	}
	completion := firstAutomaticEvent(events, EventCompactionCompleted)
	if completion.Compaction == nil || completion.Compaction.Reason != CompactionOverflow {
		t.Fatalf("completion = %#v", completion)
	}
}

func TestRunOverflowDistinctLaterToolStepCanRecoverIndependently(t *testing.T) {
	memory := automaticMemory(t)
	settings := automaticUnknownSettings()
	settings.KeepRecentTokens = estimateMessage(model.Message{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "continue"}}}) + 1
	registry, err := tool.NewRegistry(echoTool{})
	if err != nil {
		t.Fatal(err)
	}
	fake := &automaticProvider{steps: []automaticProviderStep{
		{err: automaticOverflow()},
		{response: validCompactionResponse(model.Usage{})},
		{response: automaticToolResponse("call-1", "echo")},
		{err: automaticOverflow()},
		{response: validCompactionResponse(model.Usage{})},
		{response: automaticTextResponse("done", model.FinishStop)},
	}}
	runner := New(fake, registry, memory, automaticOptions(settings))

	var events []Event
	if err := runner.Run(context.Background(), "continue", func(event Event) { events = append(events, event) }); err != nil {
		t.Fatal(err)
	}
	if len(fake.requests) != 6 {
		t.Fatalf("provider calls = %d, want two independent overflow recoveries", len(fake.requests))
	}
	if countAutomaticEvents(events, EventCompactionCompleted) != 2 {
		t.Fatalf("events = %#v, want two completed compactions", events)
	}
}

func TestRunAutomaticDisabledSkipsThresholdAndOverflowRecovery(t *testing.T) {
	t.Run("threshold", func(t *testing.T) {
		memory := automaticLargeMemory(t)
		settings := automaticSettings()
		settings.Auto = false
		settings.WorkingWindow = 11
		settings.HardInputWindow = 12
		fake := &automaticProvider{steps: []automaticProviderStep{{response: automaticTextResponse("done", model.FinishStop)}}}
		runner := New(fake, nil, memory, automaticOptions(settings))

		if err := runner.Run(context.Background(), "continue", nil); err != nil {
			t.Fatal(err)
		}
		if len(fake.requests) != 1 || fake.requests[0].SystemPrompt != automaticNormalSystemPrompt {
			t.Fatalf("requests = %#v, want one normal request", fake.requests)
		}
	})

	t.Run("overflow", func(t *testing.T) {
		memory := automaticMemory(t)
		settings := automaticUnknownSettings()
		settings.Auto = false
		fake := &automaticProvider{steps: []automaticProviderStep{{err: automaticOverflow()}}}
		runner := New(fake, nil, memory, automaticOptions(settings))

		err := runner.Run(context.Background(), "continue", nil)
		if !errors.Is(err, provider.ErrContextOverflow) {
			t.Fatalf("Run() error = %v, want context overflow", err)
		}
		if len(fake.requests) != 1 {
			t.Fatalf("provider calls = %d, want no recovery", len(fake.requests))
		}
	})
}

func TestRunAutomaticFinishReasonLengthPersistsWithoutCompaction(t *testing.T) {
	memory := automaticMemory(t)
	settings := automaticSettings()
	options := automaticOptions(settings)
	estimate := automaticRequestEstimate(memory, options, nil, "continue")
	options.Compaction.WorkingWindow = estimate + 100 + options.Compaction.ReserveTokens
	options.Compaction.HardInputWindow = estimate + 200 + options.Compaction.ReserveTokens
	fake := &automaticProvider{steps: []automaticProviderStep{{response: automaticTextResponse("truncated", model.FinishLength)}}}
	runner := New(fake, nil, memory, options)

	var events []Event
	if err := runner.Run(context.Background(), "continue", func(event Event) { events = append(events, event) }); err != nil {
		t.Fatal(err)
	}
	messages := memory.Messages()
	assistant := messages[len(messages)-1]
	if assistant.Role != model.RoleAssistant || assistant.FinishReason != model.FinishLength || assistant.Text() != "truncated" {
		t.Fatalf("assistant = %#v", assistant)
	}
	if len(fake.requests) != 1 || countAutomaticEvents(events, EventCompactionStarted) != 0 {
		t.Fatalf("calls=%d events=%#v, want ordinary successful completion", len(fake.requests), events)
	}
}

type largeAutomaticDefinitionTool struct{}

func (largeAutomaticDefinitionTool) Definition() model.ToolDefinition {
	return model.ToolDefinition{
		Name:        "large-definition",
		Description: strings.Repeat("schema pressure ", 400),
		Parameters: map[string]any{
			"type":        "object",
			"description": strings.Repeat("parameter pressure ", 400),
		},
	}
}

func (largeAutomaticDefinitionTool) Execute(context.Context, json.RawMessage) tool.Result {
	return tool.Result{Content: "unused"}
}

type automaticProviderStep struct {
	response provider.Response
	err      error
	stream   []provider.StreamEvent
	before   func(context.Context)
}

type automaticProvider struct {
	steps    []automaticProviderStep
	requests []provider.Request
}

func (p *automaticProvider) SerializedRequestSize(request provider.Request) (int, error) {
	encoded, err := json.Marshal(request)
	return len(encoded), err
}

func (p *automaticProvider) Complete(ctx context.Context, request provider.Request, emit func(provider.StreamEvent)) (provider.Response, error) {
	p.requests = append(p.requests, testCloneRequest(request))
	if len(p.steps) == 0 {
		return provider.Response{}, errors.New("no automatic provider step")
	}
	step := p.steps[0]
	p.steps = p.steps[1:]
	if step.before != nil {
		step.before(ctx)
	}
	stream := step.stream
	if stream == nil {
		for _, block := range step.response.Message.Blocks {
			switch block.Type {
			case model.BlockText:
				stream = append(stream, provider.StreamEvent{Type: provider.StreamTextDelta, Text: block.Text})
			case model.BlockToolCall:
				stream = append(stream, provider.StreamEvent{Type: provider.StreamToolCallDelta, ToolCallID: block.ToolCallID, ToolName: block.ToolName, Arguments: string(block.Arguments)})
			}
		}
	}
	for _, event := range stream {
		emit(event)
	}
	if step.err != nil {
		return provider.Response{}, step.err
	}
	return testCloneResponse(step.response), nil
}

func automaticMemory(t *testing.T) *session.Memory {
	t.Helper()
	memory := session.NewMemory(testHeader(t))
	appendCompactionMessages(t, memory,
		compactionTextMessage("old-u1", model.RoleUser, "old request"),
		compactionTextMessage("old-a1", model.RoleAssistant, "old answer"),
		compactionTextMessage("old-u2", model.RoleUser, "recent request"),
	)
	return memory
}

func automaticLargeMemory(t *testing.T) *session.Memory {
	t.Helper()
	memory := session.NewMemory(testHeader(t))
	appendCompactionMessages(t, memory,
		compactionTextMessage("large-u1", model.RoleUser, strings.Repeat("historic request ", 400)),
		compactionTextMessage("large-a1", model.RoleAssistant, strings.Repeat("historic answer ", 400)),
		compactionTextMessage("large-u2", model.RoleUser, "recent request"),
	)
	return memory
}

func automaticSettings() CompactionSettings {
	return CompactionSettings{
		Auto: true, WorkingWindow: 10_000, HardInputWindow: 20_000,
		ReserveTokens: 10, KeepRecentTokens: 1,
	}
}

func automaticUnknownSettings() CompactionSettings {
	settings := automaticSettings()
	settings.WorkingWindow = 0
	settings.HardInputWindow = 0
	return settings
}

func automaticOptions(settings CompactionSettings) Options {
	return Options{
		Model: "automatic-test-model", SystemPrompt: automaticNormalSystemPrompt, Thinking: "high",
		Compaction: settings, Now: fixedClock, NewID: fixedIDs(),
	}
}

func automaticRequestEstimate(memory session.Session, options Options, registry *tool.Registry, userText string) int {
	messages := cloneMessages(memory.Messages())
	messages = append(messages, model.Message{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: userText}}})
	var tools []model.ToolDefinition
	if registry != nil {
		tools = registry.Definitions()
	}
	latest, hasLatest := memory.LatestCompaction()
	return estimateRequest(provider.Request{
		Model: options.Model, SystemPrompt: options.SystemPrompt, Thinking: options.Thinking,
		Messages: messages, Tools: tools,
	}, latest, hasLatest)
}

func automaticTextResponse(text string, reason model.FinishReason) provider.Response {
	return provider.Response{
		Message:      model.Message{Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: text}}},
		FinishReason: reason,
	}
}

func automaticToolResponse(callID, name string) provider.Response {
	return provider.Response{
		Message: model.Message{Role: model.RoleAssistant, Blocks: []model.Block{{
			Type: model.BlockToolCall, ToolCallID: callID, ToolName: name, Arguments: json.RawMessage(`{"value":"hello"}`),
		}}},
		FinishReason: model.FinishToolCalls,
	}
}

func automaticOverflow() error {
	return &provider.ContextOverflowError{Status: 400, Code: "context_length_exceeded"}
}

func countAutomaticEvents(events []Event, eventType EventType) int {
	count := 0
	for _, event := range events {
		if event.Type == eventType {
			count++
		}
	}
	return count
}

func firstAutomaticEvent(events []Event, eventType EventType) Event {
	for _, event := range events {
		if event.Type == eventType {
			return event
		}
	}
	return Event{}
}

func automaticEventText(events []Event) string {
	var text strings.Builder
	for _, event := range events {
		if event.Type == EventTextDelta {
			text.WriteString(event.Text)
		}
	}
	return text.String()
}

func boolInt(value bool) int {
	if value {
		return 1
	}
	return 0
}
