package agent

import (
	"context"
	"encoding/json"
	"errors"
	"math"
	"reflect"
	"strings"
	"testing"
	"time"
	"unicode/utf8"

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/provider"
	"github.com/baiyuqing/otto/internal/session"
	"github.com/baiyuqing/otto/internal/tool"
)

func TestAutomaticCompactionWaitsWithoutWarningForUnsafeRetainedTail(t *testing.T) {
	memory := session.NewMemory(testHeader(t))
	appendCompactionMessages(t, memory,
		model.Message{ID: "checkpoint-tail-0", Role: model.RoleAssistant, CreatedAt: time.Unix(1, 0).UTC(), FinishReason: model.FinishToolCalls, Blocks: []model.Block{{
			Type: model.BlockToolCall, ToolName: "read", ToolCallID: "repair", Arguments: json.RawMessage(`{"path":"a.go"}`),
		}}},
		model.Message{ID: "repair-result", Role: model.RoleTool, CreatedAt: time.Unix(2, 0).UTC(), Blocks: []model.Block{{
			Type: model.BlockToolResult, ToolName: "read", ToolCallID: "repair", Text: "tool result missing from prior session", IsError: true,
		}}},
	)
	wrapped := &retainedTailMetadataSession{Session: memory, metadata: session.CompactionMetadata{
		ID: "checkpoint", Summary: validStructuredSummary, RetainedTailOnly: true, FirstPostCheckpointMessageID: "repair-result",
	}}
	fake := &compactProvider{responses: []provider.Response{{Message: model.Message{Role: model.RoleAssistant}, FinishReason: model.FinishStop}}}
	options := testCompactionOptions()
	options.Compaction = CompactionSettings{Auto: true, WorkingWindow: 10, HardInputWindow: 10, ReserveTokens: 1, KeepRecentTokens: 1}
	runner := New(fake, nil, wrapped, options)
	manual, err := runner.Compact(context.Background(), "", nil)
	if err != nil || !manual.Noop || fake.calls != 0 || wrapped.appendCalls != 0 {
		t.Fatalf("manual unsafe retained-tail compaction = %#v, %v; calls=%d appends=%d", manual, err, fake.calls, wrapped.appendCalls)
	}
	var events []Event
	state := runDispatchState{}
	if _, err := runner.dispatchNormalProviderStep(context.Background(), func(event Event) { events = append(events, event) }, &state); err != nil {
		t.Fatalf("automatic retained-tail wait returned an error: %v", err)
	}
	if fake.calls != 1 || wrapped.appendCalls != 0 || len(events) != 2 || events[0].Type != EventCompactionStarted || events[1].Type != EventCompactionCompleted {
		t.Fatalf("provider calls=%d appends=%d events=%#v", fake.calls, wrapped.appendCalls, events)
	}
	started, completed := events[0].Compaction, events[1].Compaction
	if started == nil || started.Reason != CompactionThreshold || !started.Automatic || started.Noop ||
		completed == nil || completed.Reason != CompactionThreshold || !completed.Automatic || !completed.Noop {
		t.Fatalf("automatic no-op event sequence = %#v", events)
	}
	if countCompactEvents(events, EventCompactionWarning) != 0 || countCompactEvents(events, EventAgentError) != 0 || countCompactEvents(events, EventTextDelta) != 0 {
		t.Fatalf("automatic no-op emitted non-terminal output: %#v", events)
	}
	assertCompactionEventsContainNoText(t, events, validStructuredSummary, "a.go", "repair")
	if state.proactiveAttempted {
		t.Fatal("no-op retained-tail wait consumed the turn's future compaction opportunity")
	}
}

func TestReactiveCompactionReturnsOriginalOverflowWhenRetainedTailMustWait(t *testing.T) {
	memory := session.NewMemory(testHeader(t))
	appendCompactionMessages(t, memory,
		model.Message{ID: "checkpoint-tail-0", Role: model.RoleAssistant, CreatedAt: time.Unix(1, 0).UTC(), FinishReason: model.FinishToolCalls, Blocks: []model.Block{{
			Type: model.BlockToolCall, ToolName: "read", ToolCallID: "repair", Arguments: json.RawMessage(`{"path":"a.go"}`),
		}}},
		model.Message{ID: "repair-result", Role: model.RoleTool, CreatedAt: time.Unix(2, 0).UTC(), Blocks: []model.Block{{
			Type: model.BlockToolResult, ToolName: "read", ToolCallID: "repair", Text: "repair", IsError: true,
		}}},
	)
	wrapped := &retainedTailMetadataSession{Session: memory, metadata: session.CompactionMetadata{
		ID: "checkpoint", Summary: validStructuredSummary, RetainedTailOnly: true, FirstPostCheckpointMessageID: "repair-result",
	}}
	fake := &retainedTailOverflowProvider{}
	options := testCompactionOptions()
	options.Compaction = CompactionSettings{Auto: true, KeepRecentTokens: 1}
	runner := New(fake, nil, wrapped, options)
	var events []Event
	_, err := runner.dispatchNormalProviderStep(context.Background(), func(event Event) { events = append(events, event) }, &runDispatchState{})
	var generic *automaticDispatchError
	if !errors.Is(err, provider.ErrContextOverflow) || errors.As(err, &generic) {
		t.Fatalf("reactive wait error = %T %v; want original typed overflow", err, err)
	}
	if fake.calls != 1 || wrapped.appendCalls != 0 || len(events) != 2 || events[0].Type != EventCompactionStarted || events[1].Type != EventCompactionCompleted {
		t.Fatalf("provider calls=%d appends=%d events=%#v", fake.calls, wrapped.appendCalls, events)
	}
	started, completed := events[0].Compaction, events[1].Compaction
	if started == nil || started.Reason != CompactionOverflow || !started.Automatic || started.Noop ||
		completed == nil || completed.Reason != CompactionOverflow || !completed.Automatic || !completed.Noop {
		t.Fatalf("reactive no-op event sequence = %#v", events)
	}
	if countCompactEvents(events, EventCompactionWarning) != 0 || countCompactEvents(events, EventAgentError) != 0 || countCompactEvents(events, EventTextDelta) != 0 {
		t.Fatalf("reactive no-op emitted non-terminal output: %#v", events)
	}
	assertCompactionEventsContainNoText(t, events, validStructuredSummary, "a.go", "repair")
}

func TestCompactEmitsPlanBeforeProviderSummaryCall(t *testing.T) {
	memory := populatedCompactionMemory(t) // u1(user), a1(assistant), u2(user)
	fake := &compactProvider{responses: []provider.Response{validCompactionResponse(model.Usage{})}}
	var events []Event
	plannedBeforeProvider := false
	fake.beforeComplete = func() {
		if countCompactEvents(events, EventCompactionPlanned) == 1 {
			plannedBeforeProvider = true
		}
	}
	runner := New(fake, nil, memory, testCompactionOptions())

	result, err := runner.Compact(context.Background(), "", func(event Event) { events = append(events, event) })
	if err != nil {
		t.Fatal(err)
	}
	if !plannedBeforeProvider {
		t.Fatal("compaction plan was not emitted before the provider summary call")
	}
	if got := countCompactEvents(events, EventCompactionPlanned); got != 1 {
		t.Fatalf("planned events = %d, want 1; events=%#v", got, events)
	}
	plan := firstCompactionPlan(events)
	if plan == nil {
		t.Fatal("planned event carried no plan payload")
	}
	// The latest user group (u2) is retained; the historic prefix (u1, a1) is summarized.
	if plan.RetainedMessages != 1 || plan.SummarizedMessages != 2 {
		t.Fatalf("plan counts = retained %d summarized %d, want 1/2", plan.RetainedMessages, plan.SummarizedMessages)
	}
	if plan.Mode != CompactionModeStructured {
		t.Fatalf("plan mode = %q, want structured", plan.Mode)
	}
	if plan.Reason != CompactionManual || plan.Automatic {
		t.Fatalf("plan reason=%q automatic=%t, want manual/false", plan.Reason, plan.Automatic)
	}
	if plan.TokensBefore != result.TokensBefore || plan.TokensBefore <= 0 {
		t.Fatalf("plan tokensBefore=%d, result tokensBefore=%d", plan.TokensBefore, result.TokensBefore)
	}
	if plan.EstimatedTokensAfter <= 0 || plan.EstimatedTokensAfter >= plan.TokensBefore {
		t.Fatalf("plan estimated after=%d not a saving below before=%d", plan.EstimatedTokensAfter, plan.TokensBefore)
	}
}

func TestCompactNoopEmitsNoPlan(t *testing.T) {
	memory := session.NewMemory(testHeader(t))
	appendCompactionMessages(t, memory, compactionTextMessage("only", model.RoleUser, "nothing old enough"))
	fake := &compactProvider{}
	runner := New(fake, nil, memory, testCompactionOptions())

	var events []Event
	if _, err := runner.Compact(context.Background(), "", func(event Event) { events = append(events, event) }); err != nil {
		t.Fatal(err)
	}
	if got := countCompactEvents(events, EventCompactionPlanned); got != 0 {
		t.Fatalf("planned events = %d, want 0 on no-op; events=%#v", got, events)
	}
}

func TestCompactManualNoopEmitsBoundedCompletion(t *testing.T) {
	memory := session.NewMemory(testHeader(t))
	appendCompactionMessages(t, memory, compactionTextMessage("only", model.RoleUser, "nothing old enough"))
	fake := &compactProvider{}
	runner := New(fake, nil, memory, testCompactionOptions())

	var events []Event
	result, err := runner.Compact(context.Background(), "ignored on noop", func(event Event) { events = append(events, event) })
	if err != nil {
		t.Fatal(err)
	}
	if result != (CompactionResult{Noop: true}) {
		t.Fatalf("result = %#v", result)
	}
	if fake.calls != 0 || len(events) != 2 || events[0].Type != EventCompactionStarted || events[1].Type != EventCompactionCompleted {
		t.Fatalf("calls=%d events=%#v", fake.calls, events)
	}
	if events[0].Compaction == nil || events[0].Compaction.Reason != CompactionManual || events[1].Compaction == nil || !events[1].Compaction.Noop {
		t.Fatalf("completion = %#v", events[1])
	}
	assertCompactionEventsContainNoText(t, events, "ignored on noop", "nothing old enough")
}

func TestCompactPersistsSummaryAndEmitsUsageOnce(t *testing.T) {
	memory := populatedCompactionMemory(t)
	fake := &compactProvider{responses: []provider.Response{validCompactionResponse(model.Usage{InputTokens: 120, OutputTokens: 30, CachedInputTokens: 20})}}
	runner := New(fake, nil, memory, testCompactionOptions())

	var events []Event
	result, err := runner.Compact(context.Background(), "focus on tests", func(event Event) { events = append(events, event) })
	if err != nil {
		t.Fatal(err)
	}
	if result.Noop || result.CheckpointID == "" || !result.UsagePresent || result.Usage != (model.Usage{InputTokens: 120, OutputTokens: 30, CachedInputTokens: 20}) {
		t.Fatalf("result=%#v", result)
	}
	if countCompactEvents(events, EventCompactionCompleted) != 1 || countCompactEvents(events, EventProviderUsage) != 0 || countCompactEvents(events, EventTextDelta) != 0 {
		t.Fatalf("events=%#v", events)
	}
	completed := events[len(events)-1].Compaction
	if completed == nil || completed.CheckpointID != result.CheckpointID || completed.Usage != result.Usage || !completed.UsagePresent {
		t.Fatalf("completion=%#v result=%#v", completed, result)
	}
	if len(fake.requests) != 1 || fake.requests[0].Model != "test-model" || fake.requests[0].Thinking != "high" || fake.requests[0].Tools != nil {
		t.Fatalf("summary requests=%#v", fake.requests)
	}
	if !strings.Contains(fake.requests[0].SystemPrompt, "Additional focus:\nfocus on tests") {
		t.Fatalf("focus missing from system prompt: %q", fake.requests[0].SystemPrompt)
	}
	messages := memory.Messages()
	if len(messages) != 2 || messages[0].Role != model.RoleContext || !strings.Contains(messages[0].Text(), validStructuredSummary) || messages[1].ID != "u2" {
		t.Fatalf("compacted messages=%#v", messages)
	}
	if messages[0].Usage == nil || *messages[0].Usage != result.Usage {
		t.Fatalf("checkpoint usage=%#v", messages[0].Usage)
	}
	latest, ok := memory.LatestCompaction()
	if !ok || latest.Summary != validStructuredSummary || strings.Contains(latest.Summary, "<read-files>") || strings.Contains(latest.Summary, "<modified-files>") {
		t.Fatalf("empty file lists changed summary: %#v, %v", latest, ok)
	}
}

func TestCompactUsesExactNormalRequestEstimateAndProviderSizer(t *testing.T) {
	memory := populatedCompactionMemory(t)
	registry, err := tool.NewRegistry(echoTool{})
	if err != nil {
		t.Fatal(err)
	}
	fake := &compactProvider{responses: []provider.Response{validCompactionResponse(model.Usage{})}}
	options := testCompactionOptions()
	options.SystemPrompt = "stable system"
	runner := New(fake, registry, memory, options)
	snapshot := memory.Messages()
	wantBefore := estimateRequest(provider.Request{
		Model: options.Model, SystemPrompt: options.SystemPrompt, Thinking: options.Thinking,
		Messages: snapshot, Tools: registry.Definitions(),
	}, session.CompactionMetadata{}, false)

	result, err := runner.Compact(context.Background(), "", nil)
	if err != nil {
		t.Fatal(err)
	}
	if fake.sizeCalls != 1 {
		t.Fatalf("SerializedRequestSize calls = %d, want 1", fake.sizeCalls)
	}
	if result.TokensBefore != wantBefore || result.EstimatedTokensAfter <= 0 {
		t.Fatalf("result=%#v want tokensBefore=%d", result, wantBefore)
	}
	latest, ok := memory.LatestCompaction()
	if !ok || latest.TokensBefore != wantBefore {
		t.Fatalf("latest=%#v ok=%t", latest, ok)
	}
}

func TestCompactFirstTurnPrefixCreatesStructuredSummary(t *testing.T) {
	messages := []model.Message{
		compactionTextMessage("turn-u", model.RoleUser, "long first request"),
		compactionTextMessage("turn-early", model.RoleAssistant, strings.Repeat("early", 240)),
		compactionTextMessage("turn-late", model.RoleAssistant, "late progress"),
		compactionTextMessage("latest-u", model.RoleUser, "follow-up"),
	}
	memory := session.NewMemory(testHeader(t))
	appendCompactionMessages(t, memory, messages...)
	options := testCompactionOptions()
	options.Compaction.KeepRecentTokens = estimateMessage(messages[2]) + estimateMessage(messages[3])
	fake := &compactProvider{responses: []provider.Response{validCompactionResponse(model.Usage{})}}
	runner := New(fake, nil, memory, options)

	if _, err := runner.Compact(context.Background(), "", nil); err != nil {
		t.Fatal(err)
	}
	if len(fake.requests) != 1 || !strings.Contains(fake.requests[0].Messages[0].Text(), "<summary-mode>structured</summary-mode>") {
		t.Fatalf("requests=%#v", fake.requests)
	}
	latest, _ := memory.LatestCompaction()
	if latest.Summary != validStructuredSummary {
		t.Fatalf("summary=%q", latest.Summary)
	}
}

func TestCompactTurnPrefixPreservesPreviousStructuredSummary(t *testing.T) {
	messages := []model.Message{
		compactionTextMessage("turn-u", model.RoleUser, "long first request"),
		compactionTextMessage("turn-early", model.RoleAssistant, strings.Repeat("early", 240)),
		compactionTextMessage("turn-late", model.RoleAssistant, "late progress"),
		compactionTextMessage("latest-u", model.RoleUser, "follow-up"),
	}
	memory := session.NewMemory(testHeader(t))
	appendCompactionMessages(t, memory, messages...)
	priorSuffix := "\n\n<read-files>\nprior.go\n</read-files>"
	if _, err := memory.AppendCompaction(context.Background(), session.CompactionCheckpoint{
		Summary: validStructuredSummary + priorSuffix, FirstKeptEntryID: messages[0].ID,
		TokensBefore: 100, Details: session.CompactionDetails{ReadFiles: []string{"prior.go"}}, CreatedAt: fixedClock(),
	}); err != nil {
		t.Fatal(err)
	}
	options := testCompactionOptions()
	options.Compaction.KeepRecentTokens = estimateMessage(messages[2]) + estimateMessage(messages[3])
	fake := &compactProvider{responses: []provider.Response{{Message: summaryMessage("preserve early turn progress")}}}
	runner := New(fake, nil, memory, options)

	if _, err := runner.Compact(context.Background(), "", nil); err != nil {
		t.Fatal(err)
	}
	if len(fake.requests) != 1 || !strings.Contains(fake.requests[0].Messages[0].Text(), "<summary-mode>turn-prefix</summary-mode>") {
		t.Fatalf("requests=%#v", fake.requests)
	}
	latest, _ := memory.LatestCompaction()
	want := validStructuredSummary + splitTurnSummarySeparator + "preserve early turn progress" + priorSuffix
	if latest.Summary != want || strings.Count(latest.Summary, "<read-files>") != 1 {
		t.Fatalf("summary=%q, want %q", latest.Summary, want)
	}
}

func TestCompactTurnPrefixStripsStaleSuffixWithAbsentDetailsBeforeReuse(t *testing.T) {
	messages := []model.Message{
		compactionTextMessage("turn-u", model.RoleUser, "long first request"),
		compactionTextMessage("turn-early", model.RoleAssistant, strings.Repeat("early", 240)),
		compactionTextMessage("turn-late", model.RoleAssistant, "late progress"),
		compactionTextMessage("latest-u", model.RoleUser, "follow-up"),
	}
	memory := session.NewMemory(testHeader(t))
	appendCompactionMessages(t, memory, messages...)
	const stalePath = "stale-external.go"
	if _, err := memory.AppendCompaction(context.Background(), session.CompactionCheckpoint{
		Summary:          validStructuredSummary + "\n\n<modified-files>\n" + stalePath + "\n</modified-files>",
		FirstKeptEntryID: messages[0].ID,
		TokensBefore:     100,
		CreatedAt:        fixedClock(),
	}); err != nil {
		t.Fatal(err)
	}
	options := testCompactionOptions()
	options.Compaction.KeepRecentTokens = estimateMessage(messages[2]) + estimateMessage(messages[3])
	fake := &compactProvider{responses: []provider.Response{{Message: summaryMessage("preserve early turn progress")}}}
	runner := New(fake, nil, memory, options)

	if _, err := runner.Compact(context.Background(), "", nil); err != nil {
		t.Fatal(err)
	}
	if len(fake.requests) != 1 || strings.Contains(fake.requests[0].Messages[0].Text(), stalePath) {
		t.Fatalf("stale suffix sent to summary provider: %#v", fake.requests)
	}
	latest, _ := memory.LatestCompaction()
	want := validStructuredSummary + splitTurnSummarySeparator + "preserve early turn progress"
	if latest.Summary != want || strings.Contains(latest.Summary, stalePath) {
		t.Fatalf("persisted summary = %q, want %q", latest.Summary, want)
	}
}

func TestCompactSplitTurnMakesTwoCallsAndCombinesUsageSaturating(t *testing.T) {
	messages := []model.Message{
		compactionTextMessage("history-u", model.RoleUser, "historical request"),
		compactionTextMessage("history-a", model.RoleAssistant, "historical answer"),
		compactionTextMessage("turn-u", model.RoleUser, "long turn request"),
		compactionTextMessage("turn-early", model.RoleAssistant, strings.Repeat("early", 240)),
		compactionTextMessage("turn-late", model.RoleAssistant, "late progress"),
		compactionTextMessage("latest-u", model.RoleUser, "follow-up"),
	}
	memory := session.NewMemory(testHeader(t))
	appendCompactionMessages(t, memory, messages...)
	keep := estimateMessage(messages[4]) + estimateMessage(messages[5])
	fake := &compactProvider{responses: []provider.Response{
		validCompactionResponse(model.Usage{InputTokens: math.MaxInt - 2, OutputTokens: 7, CachedInputTokens: 4}),
		{Message: summaryMessage("retain the early turn context"), Usage: model.Usage{InputTokens: 10, OutputTokens: math.MaxInt, CachedInputTokens: 6}},
	}}
	options := testCompactionOptions()
	options.Compaction.KeepRecentTokens = keep
	runner := New(fake, nil, memory, options)

	result, err := runner.Compact(context.Background(), "", nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(fake.requests) != 2 || !strings.Contains(fake.requests[0].Messages[0].Text(), "<summary-mode>structured</summary-mode>") || !strings.Contains(fake.requests[1].Messages[0].Text(), "<summary-mode>turn-prefix</summary-mode>") {
		t.Fatalf("requests=%#v", fake.requests)
	}
	wantUsage := model.Usage{InputTokens: math.MaxInt, OutputTokens: math.MaxInt, CachedInputTokens: 10}
	if !result.UsagePresent || result.Usage != wantUsage {
		t.Fatalf("usage=%#v present=%t", result.Usage, result.UsagePresent)
	}
	latest, _ := memory.LatestCompaction()
	if latest.Summary != validStructuredSummary+splitTurnSummarySeparator+"retain the early turn context" {
		t.Fatalf("combined summary=%q", latest.Summary)
	}
}

func TestCompactUpdatesPreviousSummaryWithoutResummarizingCheckpoint(t *testing.T) {
	memory := populatedCompactionMemory(t)
	fake := &compactProvider{responses: []provider.Response{
		validCompactionResponse(model.Usage{InputTokens: 1, OutputTokens: 1}),
		validCompactionResponse(model.Usage{InputTokens: 2, OutputTokens: 2}),
	}}
	runner := New(fake, nil, memory, testCompactionOptions())
	if _, err := runner.Compact(context.Background(), "", nil); err != nil {
		t.Fatal(err)
	}
	appendCompactionMessages(t, memory,
		compactionTextMessage("a2", model.RoleAssistant, "new answer"),
		compactionTextMessage("u3", model.RoleUser, "new request"),
	)
	if _, err := runner.Compact(context.Background(), "", nil); err != nil {
		t.Fatal(err)
	}
	if len(fake.requests) != 2 {
		t.Fatalf("requests=%d", len(fake.requests))
	}
	second := fake.requests[1].Messages[0].Text()
	if !strings.Contains(second, "<previous-summary>") || !strings.Contains(second, `## Goal\nkeep working`) || strings.Contains(second, "[Compaction summary]") {
		t.Fatalf("second summary input=%q", second)
	}
}

func TestCompactAppendsFinalFileDetailsAfterUnionRedactionAndModifiedWins(t *testing.T) {
	const secret = "SECRET-FILE-PATH"
	memory := session.NewMemory(testHeader(t))
	appendCompactionMessages(t, memory,
		compactionTextMessage("old-u", model.RoleUser, "old request"),
		compactionTextMessage("old-a", model.RoleAssistant, "old answer"),
	)
	if _, err := memory.AppendCompaction(context.Background(), session.CompactionCheckpoint{
		Summary: validStructuredSummary, FirstKeptEntryID: "old-u", TokensBefore: 100,
		Details:   session.CompactionDetails{ReadFiles: []string{"prior.go", "same.go", secret + "/prior.go"}, OmittedReadFiles: 2},
		CreatedAt: fixedClock(),
	}); err != nil {
		t.Fatal(err)
	}
	appendCompactionMessages(t, memory,
		model.Message{ID: "calls", Role: model.RoleAssistant, CreatedAt: time.Unix(2, 0).UTC(), FinishReason: model.FinishToolCalls, Blocks: []model.Block{
			{Type: model.BlockToolCall, ToolName: "read", ToolCallID: "read", Arguments: json.RawMessage(`{"path":"same.go"}`)},
			{Type: model.BlockToolCall, ToolName: "edit", ToolCallID: "edit", Arguments: json.RawMessage(`{"path":"same.go","oldText":"x","newText":"y"}`)},
			{Type: model.BlockToolCall, ToolName: "write", ToolCallID: "write", Arguments: json.RawMessage(`{"path":"SECRET-FILE-PATH/new.go","content":"x"}`)},
		}},
		model.Message{ID: "results", Role: model.RoleTool, CreatedAt: time.Unix(3, 0).UTC(), Blocks: []model.Block{
			{Type: model.BlockToolResult, ToolName: "read", ToolCallID: "read", Text: "ok"},
			{Type: model.BlockToolResult, ToolName: "edit", ToolCallID: "edit", Text: "ok"},
			{Type: model.BlockToolResult, ToolName: "write", ToolCallID: "write", Text: "ok"},
		}},
		compactionTextMessage("latest-u", model.RoleUser, "continue"),
	)
	fake := &compactProvider{responses: []provider.Response{validCompactionResponse(model.Usage{})}}
	options := testCompactionOptions()
	runner := New(fake, nil, memory, options, NewRedactor([]string{secret}))
	var events []Event
	result, err := runner.Compact(context.Background(), "", func(event Event) { events = append(events, event) })
	if err != nil {
		t.Fatal(err)
	}

	latest, ok := memory.LatestCompaction()
	if !ok {
		t.Fatal("missing persisted compaction")
	}
	wantDetails := session.CompactionDetails{
		ReadFiles: []string{"prior.go", redactionMarker + "/prior.go"}, ModifiedFiles: []string{"same.go", redactionMarker + "/new.go"},
		OmittedReadFiles: 2,
	}
	if !reflect.DeepEqual(latest.Details, wantDetails) {
		t.Fatalf("details = %#v, want %#v", latest.Details, wantDetails)
	}
	wantSummary := validStructuredSummary + "\n\n<read-files>\nprior.go\n" + redactionMarker + "/prior.go\n</read-files>\n\n<modified-files>\nsame.go\n" + redactionMarker + "/new.go\n</modified-files>"
	if latest.Summary != wantSummary || strings.Contains(latest.Summary, secret) {
		t.Fatalf("persisted summary = %q, want %q", latest.Summary, wantSummary)
	}
	retained := []model.Message{compactionTextMessage("latest-u", model.RoleUser, "continue")}
	if want := estimateCompactedContext(options, nil, wantSummary, retained); result.EstimatedTokensAfter != want {
		t.Fatalf("estimated after = %d, want final-summary estimate %d", result.EstimatedTokensAfter, want)
	}
	assertCompactionEventsContainNoText(t, events, secret, latest.Summary)
}

func TestCompactCompleteSummaryExactBoundPassesAndPlusOneFailsBeforeAppend(t *testing.T) {
	detailsSuffix := "\n\n<read-files>\na.go\n</read-files>"
	base := validStructuredSummary + "\n" + strings.Repeat("x", summaryMaximumBytes-len(validStructuredSummary)-len(detailsSuffix)-1)
	for _, test := range []struct {
		name        string
		summary     string
		wantErr     bool
		wantAppends int
	}{
		{name: "exact bound", summary: base, wantAppends: 1},
		{name: "one byte over", summary: base + "x", wantErr: true},
	} {
		t.Run(test.name, func(t *testing.T) {
			memory := session.NewMemory(testHeader(t))
			appendCompactionMessages(t, memory,
				compactionTextMessage("old-u", model.RoleUser, "old request"),
				model.Message{ID: "call", Role: model.RoleAssistant, CreatedAt: time.Unix(2, 0).UTC(), FinishReason: model.FinishToolCalls, Blocks: []model.Block{{
					Type: model.BlockToolCall, ToolName: "read", ToolCallID: "read", Arguments: json.RawMessage(`{"path":"a.go"}`),
				}}},
				model.Message{ID: "result", Role: model.RoleTool, CreatedAt: time.Unix(3, 0).UTC(), Blocks: []model.Block{{
					Type: model.BlockToolResult, ToolName: "read", ToolCallID: "read", Text: "ok",
				}}},
				compactionTextMessage("latest-u", model.RoleUser, "continue"),
			)
			wrapped := &compactionHookSession{Session: memory}
			fake := &compactProvider{responses: []provider.Response{{Message: summaryMessage(test.summary)}}}
			runner := New(fake, nil, wrapped, testCompactionOptions())
			result, err := runner.Compact(context.Background(), "", nil)
			if test.wantErr {
				if !errors.Is(err, ErrInvalidCompactionSummary) || result.CheckpointID != "" {
					t.Fatalf("Compact() = %#v, %v; want typed invalid summary", result, err)
				}
			} else {
				if err != nil {
					t.Fatal(err)
				}
				latest, _ := memory.LatestCompaction()
				if len(latest.Summary) != summaryMaximumBytes || latest.Summary != base+detailsSuffix || !utf8.ValidString(latest.Summary) {
					t.Fatalf("exact persisted summary = %d bytes", len(latest.Summary))
				}
			}
			if wrapped.appendCalls != test.wantAppends {
				t.Fatalf("session appends = %d, want %d", wrapped.appendCalls, test.wantAppends)
			}
		})
	}
}

func TestCompactEstimateAfterIgnoresRetainedAssistantUsageAnchor(t *testing.T) {
	options := testCompactionOptions()
	options.SystemPrompt = "system"
	retained := []model.Message{{
		Role: model.RoleAssistant, Blocks: []model.Block{{Type: model.BlockText, Text: "retained"}},
		Usage: &model.Usage{InputTokens: 500_000},
	}}

	if got := estimateCompactedContext(options, nil, "state", retained); got != 33 {
		t.Fatalf("estimateCompactedContext() = %d, want content fallback 33", got)
	}
}

func TestCompactRejectsContradictorySummaryFinishReasons(t *testing.T) {
	for _, test := range []struct {
		name    string
		message model.FinishReason
		outer   model.FinishReason
	}{
		{name: "message stop outer tool calls", message: model.FinishStop, outer: model.FinishToolCalls},
		{name: "message tool calls outer stop", message: model.FinishToolCalls, outer: model.FinishStop},
		{name: "message stop outer length", message: model.FinishStop, outer: model.FinishLength},
	} {
		t.Run(test.name, func(t *testing.T) {
			memory := populatedCompactionMemory(t)
			response := validCompactionResponse(model.Usage{})
			response.Message.FinishReason = test.message
			response.FinishReason = test.outer
			runner := New(&compactProvider{responses: []provider.Response{response}}, nil, memory, testCompactionOptions())

			if _, err := runner.Compact(context.Background(), "", nil); !errors.Is(err, ErrInvalidCompactionSummary) {
				t.Fatalf("Compact() error = %v, want ErrInvalidCompactionSummary", err)
			}
		})
	}
}

func TestCompactRejectsInvalidResponsesWithStableBoundedIdentity(t *testing.T) {
	toolCalling := summaryMessage(validStructuredSummary)
	toolCalling.Blocks = append(toolCalling.Blocks, model.Block{Type: model.BlockToolCall, ToolName: "bash", ToolCallID: "secret-path"})
	oversized := validStructuredSummary + "\n" + strings.Repeat("x", summaryMaximumBytes-len(validStructuredSummary))
	for _, test := range []struct {
		name     string
		response provider.Response
	}{
		{name: "malformed headings", response: provider.Response{Message: summaryMessage("## Goal\nmissing sections")}},
		{name: "empty", response: provider.Response{Message: summaryMessage(" \n\t")}},
		{name: "tool calling", response: provider.Response{Message: toolCalling, FinishReason: model.FinishToolCalls}},
		{name: "oversized", response: provider.Response{Message: summaryMessage(oversized)}},
	} {
		t.Run(test.name, func(t *testing.T) {
			memory := populatedCompactionMemory(t)
			fake := &compactProvider{responses: []provider.Response{test.response}}
			runner := New(fake, nil, memory, testCompactionOptions())
			before := memory.Messages()
			var events []Event
			_, err := runner.Compact(context.Background(), "SECRET-FOCUS", func(event Event) { events = append(events, event) })
			if !errors.Is(err, ErrInvalidCompactionSummary) {
				t.Fatalf("error=%v, want ErrInvalidCompactionSummary", err)
			}
			if strings.Contains(err.Error(), "SECRET-FOCUS") || strings.Contains(err.Error(), "secret-path") || len(err.Error()) > 256 {
				t.Fatalf("leaking/unbounded error=%q", err)
			}
			if got := memory.Messages(); !equalCompactionMessageIDs(got, before) {
				t.Fatalf("session mutated: %#v", got)
			}
			assertCompactionEventsContainNoText(t, events, "SECRET-FOCUS", "secret-path", oversized)
		})
	}
}

func TestCompactCancelsProviderWhenHiddenStreamExceedsBound(t *testing.T) {
	memory := populatedCompactionMemory(t)
	fake := &streamLimitProvider{}
	runner := New(fake, nil, memory, testCompactionOptions())
	_, err := runner.Compact(context.Background(), "", nil)
	if !errors.Is(err, ErrInvalidCompactionSummary) {
		t.Fatalf("error=%v, want ErrInvalidCompactionSummary", err)
	}
	if !fake.sawCancellation {
		t.Fatal("summary provider child context was not canceled at stream bound")
	}
}

func TestCompactRejectsOversizedRequestBeforeProvider(t *testing.T) {
	memory := populatedCompactionMemory(t)
	fake := &compactProvider{responses: []provider.Response{validCompactionResponse(model.Usage{})}}
	options := testCompactionOptions()
	options.RequestSizer = compactRequestSizerFunc(func(provider.Request) (int, error) { return summaryRequestMaximumBytes + 1, nil })
	runner := New(fake, nil, memory, options)
	_, err := runner.Compact(context.Background(), "", nil)
	if !errors.Is(err, ErrInvalidCompactionSummary) || fake.calls != 0 {
		t.Fatalf("error=%v provider calls=%d", err, fake.calls)
	}
}

func TestCompactAllowsProviderInternalRetryBeforeOutput(t *testing.T) {
	memory := populatedCompactionMemory(t)
	fake := &internalRetryProvider{}
	runner := New(fake, nil, memory, testCompactionOptions())
	if _, err := runner.Compact(context.Background(), "", nil); err != nil {
		t.Fatal(err)
	}
	if fake.transportAttempts != 2 || fake.completeCalls != 1 {
		t.Fatalf("transport attempts=%d Complete calls=%d", fake.transportAttempts, fake.completeCalls)
	}
}

func TestCompactUsageAbsentDoesNotPersistOrEmitUsage(t *testing.T) {
	memory := populatedCompactionMemory(t)
	fake := &compactProvider{responses: []provider.Response{{Message: summaryMessage(validStructuredSummary)}}}
	runner := New(fake, nil, memory, testCompactionOptions())
	var completed *CompactionEvent
	result, err := runner.Compact(context.Background(), "", func(event Event) {
		if event.Type == EventCompactionCompleted {
			completed = event.Compaction
		}
	})
	if err != nil {
		t.Fatal(err)
	}
	if result.UsagePresent || result.Usage != (model.Usage{}) || completed == nil || completed.UsagePresent {
		t.Fatalf("result=%#v completion=%#v", result, completed)
	}
	latest, _ := memory.LatestCompaction()
	if latest.Usage != nil || memory.Messages()[0].Usage != nil {
		t.Fatalf("usage persisted: latest=%#v message=%#v", latest.Usage, memory.Messages()[0].Usage)
	}
}

func TestCompactRedactsSummaryBeforeValidationPersistenceAndEvents(t *testing.T) {
	const secret = "summary-secret-value"
	summary := strings.Replace(validStructuredSummary, "keep working", "keep "+secret+" working", 1)
	memory := populatedCompactionMemory(t)
	fake := &compactProvider{responses: []provider.Response{{Message: summaryMessage(summary)}}}
	runner := New(fake, nil, memory, testCompactionOptions(), NewRedactor([]string{secret}))
	var events []Event
	if _, err := runner.Compact(context.Background(), secret, func(event Event) { events = append(events, event) }); err != nil {
		t.Fatal(err)
	}
	if len(fake.requests) != 1 || strings.Contains(fake.requests[0].SystemPrompt, secret) || strings.Contains(fake.requests[0].Messages[0].Text(), secret) {
		t.Fatalf("summary request leaked secret: %#v", fake.requests)
	}
	latest, _ := memory.LatestCompaction()
	if strings.Contains(latest.Summary, secret) || !strings.Contains(latest.Summary, redactionMarker) {
		t.Fatalf("persisted summary=%q", latest.Summary)
	}
	assertCompactionEventsContainNoText(t, events, secret, latest.Summary)
}

func TestCompactIncompleteRedactionSnapshotFailsClosedWithoutProviderOrSessionMutation(t *testing.T) {
	const omitted = "omitted-compaction-message-id"
	memory := session.NewMemory(testHeader(t))
	appendCompactionMessages(t, memory,
		compactionTextMessage("old-"+omitted, model.RoleUser, "old request"),
		compactionTextMessage("old-a", model.RoleAssistant, "old answer"),
		compactionTextMessage("latest-u", model.RoleUser, "latest request"),
	)
	before := memory.Messages()
	wrapped := &compactionHookSession{Session: memory}
	fake := &compactProvider{responses: []provider.Response{validCompactionResponse(model.Usage{})}}
	runner := New(fake, nil, wrapped, testCompactionOptions(), NewRedactorWithCompleteness([]string{"known-secret"}, false))

	var events []Event
	result, err := runner.Compact(context.Background(), omitted, func(event Event) { events = append(events, event) })
	if err != nil {
		t.Fatal(err)
	}
	if !result.Noop || fake.calls != 0 || wrapped.appendCalls != 0 {
		t.Fatalf("result=%#v provider calls=%d appends=%d", result, fake.calls, wrapped.appendCalls)
	}
	if !reflect.DeepEqual(memory.Messages(), before) {
		t.Fatal("incomplete-redaction compaction mutated the session")
	}
	if len(events) != 2 || events[0].Type != EventCompactionStarted || events[1].Type != EventCompactionCompleted || events[1].Compaction == nil || !events[1].Compaction.Noop {
		t.Fatalf("events=%#v", events)
	}
	assertCompactionEventsContainNoText(t, events, omitted)
}

func TestCompactIncompleteRedactionPreservesPreCanceledContext(t *testing.T) {
	memory := populatedCompactionMemory(t)
	wrapped := &compactionHookSession{Session: memory}
	fake := &compactProvider{responses: []provider.Response{validCompactionResponse(model.Usage{})}}
	runner := New(fake, nil, wrapped, testCompactionOptions(), NewRedactorWithCompleteness(nil, false))
	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	var events []Event
	result, err := runner.Compact(ctx, "safe focus", func(event Event) { events = append(events, event) })
	if !errors.Is(err, context.Canceled) || result.CheckpointID != "" || fake.calls != 0 || wrapped.appendCalls != 0 {
		t.Fatalf("result=%#v error=%v provider calls=%d appends=%d", result, err, fake.calls, wrapped.appendCalls)
	}
	if len(events) != 2 || events[0].Type != EventCompactionStarted || events[1].Type != EventAgentError {
		t.Fatalf("events=%#v", events)
	}
}

func TestCompactCancellationBeforeAppendDoesNotCommit(t *testing.T) {
	memory := populatedCompactionMemory(t)
	wrapped := &compactionHookSession{Session: memory}
	fake := &compactProvider{responses: []provider.Response{validCompactionResponse(model.Usage{InputTokens: 2, OutputTokens: 1})}}
	ctx, cancel := context.WithCancel(context.Background())
	fake.afterComplete = cancel
	runner := New(fake, nil, wrapped, testCompactionOptions())
	before := memory.Messages()
	result, err := runner.Compact(ctx, "", nil)
	if !errors.Is(err, context.Canceled) || result.CheckpointID != "" || wrapped.appendCalls != 0 {
		t.Fatalf("result=%#v error=%v appendCalls=%d", result, err, wrapped.appendCalls)
	}
	if !equalCompactionMessageIDs(memory.Messages(), before) {
		t.Fatal("canceled compaction mutated session")
	}
}

func TestCompactCancellationImmediatelyAfterCommittedAppendReturnsCommittedResult(t *testing.T) {
	memory := populatedCompactionMemory(t)
	ctx, cancel := context.WithCancel(context.Background())
	wrapped := &compactionHookSession{Session: memory, afterAppend: cancel}
	fake := &compactProvider{responses: []provider.Response{validCompactionResponse(model.Usage{InputTokens: 2, OutputTokens: 1})}}
	runner := New(fake, nil, wrapped, testCompactionOptions())
	var events []Event
	result, err := runner.Compact(ctx, "", func(event Event) { events = append(events, event) })
	if !errors.Is(err, context.Canceled) || result.CheckpointID == "" || result.Noop {
		t.Fatalf("result=%#v error=%v", result, err)
	}
	if wrapped.appendCalls != 1 || countCompactEvents(events, EventCompactionCompleted) != 1 || countCompactEvents(events, EventAgentError) != 1 {
		t.Fatalf("appendCalls=%d events=%#v", wrapped.appendCalls, events)
	}
	completed := events[len(events)-2].Compaction
	if completed == nil || completed.CheckpointID != result.CheckpointID {
		t.Fatalf("committed completion=%#v result=%#v", completed, result)
	}
	latest, ok := memory.LatestCompaction()
	if !ok || latest.ID != result.CheckpointID {
		t.Fatalf("latest=%#v ok=%t", latest, ok)
	}
}

func TestCompactRedactorPreservesInvalidSummaryAndCancellationIdentities(t *testing.T) {
	redactor := NewRedactor([]string{"secret"})
	for _, test := range []struct {
		err  error
		want error
	}{
		{err: fmtCompactionError(ErrInvalidCompactionSummary, "secret"), want: ErrInvalidCompactionSummary},
		{err: fmtCompactionError(context.Canceled, "secret"), want: context.Canceled},
	} {
		got := redactor.RedactError(test.err)
		if !errors.Is(got, test.want) || strings.Contains(got.Error(), "secret") {
			t.Fatalf("RedactError(%v)=%v want identity %v", test.err, got, test.want)
		}
	}
}

func testCompactionOptions() Options {
	return Options{
		Model: "test-model", Thinking: "high", SystemPrompt: "normal system",
		Compaction: CompactionSettings{KeepRecentTokens: 1},
		Now:        fixedClock, NewID: fixedIDs(),
	}
}

func populatedCompactionMemory(t *testing.T) *session.Memory {
	t.Helper()
	memory := session.NewMemory(testHeader(t))
	appendCompactionMessages(t, memory,
		compactionTextMessage("u1", model.RoleUser, "old request"),
		compactionTextMessage("a1", model.RoleAssistant, "old answer"),
		compactionTextMessage("u2", model.RoleUser, "latest request"),
	)
	return memory
}

func appendCompactionMessages(t *testing.T, memory session.Session, messages ...model.Message) {
	t.Helper()
	for _, message := range messages {
		if err := memory.Append(context.Background(), message); err != nil {
			t.Fatal(err)
		}
	}
}

func compactionTextMessage(id string, role model.Role, text string) model.Message {
	message := model.Message{ID: id, Role: role, CreatedAt: time.Unix(1, 0).UTC(), Blocks: []model.Block{{Type: model.BlockText, Text: text}}}
	if role == model.RoleAssistant {
		message.FinishReason = model.FinishStop
	}
	return message
}

func validCompactionResponse(usage model.Usage) provider.Response {
	return provider.Response{Message: summaryMessage(validStructuredSummary), FinishReason: model.FinishStop, Usage: usage}
}

type compactProvider struct {
	responses      []provider.Response
	requests       []provider.Request
	calls          int
	sizeCalls      int
	beforeComplete func()
	afterComplete  func()
}

func (p *compactProvider) SerializedRequestSize(request provider.Request) (int, error) {
	p.sizeCalls++
	encoded, err := json.Marshal(request)
	return len(encoded), err
}

func (p *compactProvider) Complete(ctx context.Context, request provider.Request, emit func(provider.StreamEvent)) (provider.Response, error) {
	if p.beforeComplete != nil {
		p.beforeComplete()
	}
	p.calls++
	p.requests = append(p.requests, testCloneRequest(request))
	if len(p.responses) == 0 {
		return provider.Response{}, errors.New("no compact response")
	}
	response := testCloneResponse(p.responses[0])
	p.responses = p.responses[1:]
	for _, block := range response.Message.Blocks {
		switch block.Type {
		case model.BlockText:
			emit(provider.StreamEvent{Type: provider.StreamTextDelta, Text: block.Text})
		case model.BlockToolCall:
			emit(provider.StreamEvent{Type: provider.StreamToolCallDelta, ToolName: block.ToolName, ToolCallID: block.ToolCallID})
		}
	}
	if p.afterComplete != nil {
		p.afterComplete()
	}
	if err := ctx.Err(); err != nil {
		return provider.Response{}, err
	}
	return response, nil
}

type compactRequestSizerFunc func(provider.Request) (int, error)

func (f compactRequestSizerFunc) SerializedRequestSize(request provider.Request) (int, error) {
	return f(request)
}

type streamLimitProvider struct {
	sawCancellation bool
}

func (p *streamLimitProvider) SerializedRequestSize(provider.Request) (int, error) { return 1, nil }
func (p *streamLimitProvider) Complete(ctx context.Context, _ provider.Request, emit func(provider.StreamEvent)) (provider.Response, error) {
	emit(provider.StreamEvent{Type: provider.StreamTextDelta, Text: strings.Repeat("x", summaryMaximumBytes+1)})
	<-ctx.Done()
	p.sawCancellation = true
	return provider.Response{}, ctx.Err()
}

type retainedTailOverflowProvider struct {
	calls int
}

func (p *retainedTailOverflowProvider) SerializedRequestSize(provider.Request) (int, error) {
	return 1, nil
}
func (p *retainedTailOverflowProvider) Complete(context.Context, provider.Request, func(provider.StreamEvent)) (provider.Response, error) {
	p.calls++
	return provider.Response{}, &provider.ContextOverflowError{Status: 400, Code: "context_length_exceeded"}
}

type internalRetryProvider struct {
	completeCalls     int
	transportAttempts int
}

func (p *internalRetryProvider) SerializedRequestSize(provider.Request) (int, error) { return 1, nil }
func (p *internalRetryProvider) Complete(_ context.Context, _ provider.Request, emit func(provider.StreamEvent)) (provider.Response, error) {
	p.completeCalls++
	p.transportAttempts++ // first transport attempt fails before output and is retried internally
	p.transportAttempts++
	emit(provider.StreamEvent{Type: provider.StreamTextDelta, Text: validStructuredSummary})
	return validCompactionResponse(model.Usage{}), nil
}

type compactionHookSession struct {
	session.Session
	appendCalls int
	afterAppend func()
}

type retainedTailMetadataSession struct {
	session.Session
	metadata    session.CompactionMetadata
	appendCalls int
}

func (s *retainedTailMetadataSession) LatestCompaction() (session.CompactionMetadata, bool) {
	return s.metadata, true
}

func (s *retainedTailMetadataSession) AppendCompaction(ctx context.Context, checkpoint session.CompactionCheckpoint) (session.CompactionMetadata, error) {
	s.appendCalls++
	return s.Session.AppendCompaction(ctx, checkpoint)
}

func (s *compactionHookSession) AppendCompaction(ctx context.Context, checkpoint session.CompactionCheckpoint) (session.CompactionMetadata, error) {
	s.appendCalls++
	metadata, err := s.Session.AppendCompaction(ctx, checkpoint)
	if s.afterAppend != nil {
		s.afterAppend()
	}
	return metadata, err
}

func firstCompactionPlan(events []Event) *CompactionPlan {
	for _, event := range events {
		if event.Type == EventCompactionPlanned {
			return event.Plan
		}
	}
	return nil
}

func countCompactEvents(events []Event, eventType EventType) int {
	count := 0
	for _, event := range events {
		if event.Type == eventType {
			count++
		}
	}
	return count
}

func assertCompactionEventsContainNoText(t *testing.T, events []Event, forbidden ...string) {
	t.Helper()
	encoded, err := json.Marshal(events)
	if err != nil {
		t.Fatal(err)
	}
	for _, text := range forbidden {
		if text != "" && strings.Contains(string(encoded), text) {
			t.Fatalf("event leaked forbidden text: %q in %s", text, encoded)
		}
	}
}

func equalCompactionMessageIDs(left, right []model.Message) bool {
	if len(left) != len(right) {
		return false
	}
	for index := range left {
		if left[index].ID != right[index].ID || left[index].Text() != right[index].Text() {
			return false
		}
	}
	return true
}

func fmtCompactionError(identity error, secret string) error {
	return &compactionWrappedError{identity: identity, message: "failure " + secret}
}

type compactionWrappedError struct {
	identity error
	message  string
}

func (e *compactionWrappedError) Error() string { return e.message }
func (e *compactionWrappedError) Unwrap() error { return e.identity }
