package agent

import (
	"errors"
	"reflect"
	"testing"

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/session"
)

func TestSelectCompactionOrdinaryTurnsAndLatestUserRetention(t *testing.T) {
	messages := []model.Message{
		selectTextMessage("u1", model.RoleUser, "first request"),
		selectTextMessage("a1", model.RoleAssistant, "first answer"),
		selectTextMessage("u2", model.RoleUser, "latest request"),
		selectTextMessage("a2", model.RoleAssistant, "latest answer"),
	}

	selection, err := selectCompaction(messages, session.CompactionMetadata{}, false, 1, 0)
	if err != nil {
		t.Fatal(err)
	}
	assertSelectIDs(t, selection.HistoricalSource, "u1", "a1")
	assertSelectIDs(t, selection.Retained, "u2", "a2")
	if selection.FirstKeptID != "u2" {
		t.Fatalf("first kept ID = %q, want u2", selection.FirstKeptID)
	}
	if selection.SplitTurn {
		t.Fatal("ordinary turn boundary marked as split")
	}
}

func TestSelectCompactionNeverSplitsToolCallResults(t *testing.T) {
	messages := selectToolConversationFixture()
	forced := estimateMessage(messages[5]) + estimateMessage(messages[6])
	keep := forced + estimateMessage(messages[4]) + 1

	selection, err := selectCompaction(messages, session.CompactionMetadata{}, false, keep, 0)
	if err != nil {
		t.Fatal(err)
	}
	assertSelectIDs(t, selection.Retained, "calls", "result-2", "result-1", "answer", "u2", "a2")
	if selection.Retained[0].Role == model.RoleTool {
		t.Fatal("retained suffix starts with tool result")
	}
	if err := validateRetainedToolPairs(selection.Retained); err != nil {
		t.Fatalf("retained context invalid: %v", err)
	}

	for keep := 1; keep <= selectMessageTokens(messages)+10; keep++ {
		selection, err := selectCompaction(messages, session.CompactionMetadata{}, false, keep, 0)
		if errors.Is(err, ErrNothingToCompact) {
			continue
		}
		if err != nil {
			t.Fatalf("keep %d: %v", keep, err)
		}
		if selection.Retained[0].Role == model.RoleTool {
			t.Fatalf("keep %d starts with tool result", keep)
		}
		if err := validateRetainedToolPairs(selection.Retained); err != nil {
			t.Fatalf("keep %d retained context invalid: %v", keep, err)
		}
		assertToolAtomSide(t, selection, "calls", "result-2", "result-1")
	}
}

func TestSelectCompactionRejectsInvalidOrUnresolvedToolPairing(t *testing.T) {
	call := selectToolCallMessage("calls", model.Block{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "read"})
	result := selectToolResultMessage("result", "call-1", "read")
	tests := map[string][]model.Message{
		"unmatched result": {
			selectTextMessage("u1", model.RoleUser, "request"),
			result,
		},
		"unresolved call": {
			selectTextMessage("u1", model.RoleUser, "request"),
			call,
		},
		"partial result sequence": {
			selectTextMessage("u1", model.RoleUser, "request"),
			selectToolCallMessage("calls",
				model.Block{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "read"},
				model.Block{Type: model.BlockToolCall, ToolCallID: "call-2", ToolName: "write"},
			),
			result,
			selectTextMessage("u2", model.RoleUser, "interposed"),
		},
		"mismatched name": {
			selectTextMessage("u1", model.RoleUser, "request"),
			call,
			selectToolResultMessage("result", "call-1", "write"),
		},
	}

	for name, messages := range tests {
		t.Run(name, func(t *testing.T) {
			if _, err := selectCompaction(messages, session.CompactionMetadata{}, false, 1, 0); err == nil {
				t.Fatal("selectCompaction accepted invalid pairing")
			}
			if err := validateRetainedToolPairs(messages); err == nil {
				t.Fatal("validateRetainedToolPairs accepted invalid pairing")
			}
		})
	}
}

func TestSelectCompactionAttachesContextToFollowingMessage(t *testing.T) {
	messages := []model.Message{
		selectTextMessage("u1", model.RoleUser, "old request"),
		selectTextMessage("a1", model.RoleAssistant, "old answer"),
		selectContextMessage("ctx", "branch_summary", "adjacent context"),
		selectTextMessage("u2", model.RoleUser, "latest request"),
	}

	selection, err := selectCompaction(messages, session.CompactionMetadata{}, false, 1, 0)
	if err != nil {
		t.Fatal(err)
	}
	assertSelectIDs(t, selection.HistoricalSource, "u1", "a1")
	assertSelectIDs(t, selection.Retained, "ctx", "u2")
	if selection.FirstKeptID != "ctx" {
		t.Fatalf("first kept ID = %q, want attached context ID ctx", selection.FirstKeptID)
	}
}

func TestSelectCompactionSplitsOlderLongTurn(t *testing.T) {
	messages := []model.Message{
		selectTextMessage("history-u", model.RoleUser, "historical request"),
		selectTextMessage("history-a", model.RoleAssistant, "historical answer"),
		selectTextMessage("turn-u", model.RoleUser, "long turn request"),
		selectTextMessage("turn-early", model.RoleAssistant, selectLongText(240)),
		selectTextMessage("turn-late", model.RoleAssistant, "late progress"),
		selectTextMessage("latest-u", model.RoleUser, "follow-up"),
	}
	keep := estimateMessage(messages[4]) + estimateMessage(messages[5])

	selection, err := selectCompaction(messages, session.CompactionMetadata{}, false, keep, 0)
	if err != nil {
		t.Fatal(err)
	}
	assertSelectIDs(t, selection.HistoricalSource, "history-u", "history-a")
	assertSelectIDs(t, selection.TurnPrefixSource, "turn-u", "turn-early")
	assertSelectIDs(t, selection.Retained, "turn-late", "latest-u")
	if !selection.SplitTurn {
		t.Fatal("selection did not mark split turn")
	}
}

func TestSelectCompactionUsesPreviousSummaryWithoutResummarizingItsContext(t *testing.T) {
	messages := []model.Message{
		selectCompactionMessage("checkpoint", "[Compaction summary]\nprior structured summary"),
		selectTextMessage("old-u", model.RoleUser, "retained old request"),
		selectTextMessage("old-a", model.RoleAssistant, "retained old answer"),
		selectTextMessage("latest-u", model.RoleUser, "new request"),
	}
	latest := session.CompactionMetadata{ID: "checkpoint", Summary: "metadata fallback must not win"}

	selection, err := selectCompaction(messages, latest, true, 1, 0)
	if err != nil {
		t.Fatal(err)
	}
	if selection.PreviousSummary != "prior structured summary" {
		t.Fatalf("previous summary = %q", selection.PreviousSummary)
	}
	assertSelectIDs(t, selection.HistoricalSource, "old-u", "old-a")
	assertSelectIDs(t, selection.Retained, "latest-u")
	for _, source := range append(append([]model.Message{}, selection.HistoricalSource...), selection.TurnPrefixSource...) {
		if source.ContextType == "compaction" {
			t.Fatal("previous compaction context was included in transcript source")
		}
	}
}

func TestSelectCompactionRetainedTailOnlyRequiresGenuinePostCheckpointID(t *testing.T) {
	messages := []model.Message{
		selectCompactionMessage("checkpoint", "[Compaction summary]\nprior"),
		selectTextMessage("checkpoint-tail-0", model.RoleUser, "synthetic retained request"),
		selectTextMessage("checkpoint-tail-1", model.RoleAssistant, "synthetic retained answer"),
		selectTextMessage("post-user", model.RoleUser, "genuine request"),
		selectTextMessage("post-assistant", model.RoleAssistant, "genuine answer"),
	}
	latest := session.CompactionMetadata{
		ID:                           "checkpoint",
		RetainedTailOnly:             true,
		FirstPostCheckpointMessageID: "post-user",
	}

	selection, err := selectCompaction(messages, latest, true, 100000, 0)
	if err != nil {
		t.Fatal(err)
	}
	assertSelectIDs(t, selection.HistoricalSource, "checkpoint-tail-0", "checkpoint-tail-1")
	assertSelectIDs(t, selection.Retained, "post-user", "post-assistant")
	if selection.FirstKeptID != "post-user" {
		t.Fatalf("first kept ID = %q, want exact genuine ID", selection.FirstKeptID)
	}

	for name, anchor := range map[string]string{"empty": "", "missing": "not-present"} {
		t.Run(name, func(t *testing.T) {
			metadata := latest
			metadata.FirstPostCheckpointMessageID = anchor
			if _, err := selectCompaction(messages, metadata, true, 1, 0); !errors.Is(err, ErrNothingToCompact) {
				t.Fatalf("error = %v, want ErrNothingToCompact", err)
			}
		})
	}
}

func TestSelectCompactionRetainedTailAdvancesPastSyntheticToolAtom(t *testing.T) {
	messages := []model.Message{
		selectCompactionMessage("checkpoint", "[Compaction summary]\nprior"),
		selectToolCallMessage("checkpoint-tail-0", model.Block{Type: model.BlockToolCall, ToolCallID: "repair", ToolName: "read"}),
		selectToolResultMessage("repair-result", "repair", "read"),
		selectTextMessage("later-user", model.RoleUser, "safe request"),
		selectTextMessage("later-assistant", model.RoleAssistant, "safe answer"),
	}
	latest := session.CompactionMetadata{ID: "checkpoint", RetainedTailOnly: true, FirstPostCheckpointMessageID: "repair-result"}

	selection, err := selectCompaction(messages, latest, true, 100000, 0)
	if err != nil {
		t.Fatal(err)
	}
	assertSelectIDs(t, selection.HistoricalSource, "checkpoint-tail-0", "repair-result")
	assertSelectIDs(t, selection.Retained, "later-user", "later-assistant")
	if selection.FirstKeptID != "later-user" {
		t.Fatalf("first kept ID = %q, want later-user", selection.FirstKeptID)
	}

	if _, err := selectCompaction(messages[:3], latest, true, 1, 0); !errors.Is(err, ErrNothingToCompact) {
		t.Fatalf("unsafe atom without a later real group error = %v, want ErrNothingToCompact", err)
	}
}

func TestSelectCompactionRetainedTailKeepsRealAttachedContextGroup(t *testing.T) {
	messages := []model.Message{
		selectCompactionMessage("checkpoint", "[Compaction summary]\nprior"),
		selectTextMessage("checkpoint-tail-0", model.RoleUser, "synthetic request"),
		selectTextMessage("checkpoint-tail-1", model.RoleAssistant, "synthetic answer"),
		selectContextMessage("real-context", "branch_summary", "real context"),
		selectTextMessage("real-user", model.RoleUser, "real request"),
		selectTextMessage("real-assistant", model.RoleAssistant, "real answer"),
	}
	latest := session.CompactionMetadata{ID: "checkpoint", RetainedTailOnly: true, FirstPostCheckpointMessageID: "real-context"}

	selection, err := selectCompaction(messages, latest, true, 1, 0)
	if err != nil {
		t.Fatal(err)
	}
	assertSelectIDs(t, selection.HistoricalSource, "checkpoint-tail-0", "checkpoint-tail-1")
	assertSelectIDs(t, selection.Retained, "real-context", "real-user", "real-assistant")
	if selection.FirstKeptID != "real-context" {
		t.Fatalf("first kept ID = %q, want attached context", selection.FirstKeptID)
	}
}

func TestSelectCompactionEmptySyntheticTailEventuallyUsesNormalRecentSelection(t *testing.T) {
	latest := session.CompactionMetadata{ID: "checkpoint", RetainedTailOnly: true, FirstPostCheckpointMessageID: "real-u1"}
	short := []model.Message{
		selectCompactionMessage("checkpoint", "[Compaction summary]\nprior"),
		selectTextMessage("real-u1", model.RoleUser, "first real request"),
		selectTextMessage("real-a1", model.RoleAssistant, "first real answer"),
	}
	if _, err := selectCompaction(short, latest, true, 1, 0); !errors.Is(err, ErrNothingToCompact) {
		t.Fatalf("short real history error = %v, want ErrNothingToCompact", err)
	}

	messages := append(short,
		selectTextMessage("real-u2", model.RoleUser, "later request"),
		selectTextMessage("real-a2", model.RoleAssistant, "later answer"),
	)
	selection, err := selectCompaction(messages, latest, true, 1, 0)
	if err != nil {
		t.Fatal(err)
	}
	assertSelectIDs(t, selection.HistoricalSource, "real-u1", "real-a1")
	assertSelectIDs(t, selection.Retained, "real-u2", "real-a2")
	if selection.FirstKeptID != "real-u2" {
		t.Fatalf("first kept ID = %q, want normal recent-tail cut", selection.FirstKeptID)
	}
}

func TestSelectCompactionDualFormUsesRealMessageID(t *testing.T) {
	messages := []model.Message{
		selectCompactionMessage("checkpoint", "[Compaction summary]\nprior"),
		selectTextMessage("real-u", model.RoleUser, "real retained request"),
		selectTextMessage("real-a", model.RoleAssistant, "real retained answer"),
		selectTextMessage("post-u", model.RoleUser, "post-checkpoint request"),
	}
	latest := session.CompactionMetadata{
		ID:                           "checkpoint",
		FirstKeptEntryID:             "real-u",
		FirstPostCheckpointMessageID: "post-u",
		RetainedTailOnly:             false,
	}
	keep := estimateMessage(messages[2]) + estimateMessage(messages[3])

	selection, err := selectCompaction(messages, latest, true, keep, 0)
	if err != nil {
		t.Fatal(err)
	}
	assertSelectIDs(t, selection.TurnPrefixSource, "real-u")
	assertSelectIDs(t, selection.Retained, "real-a", "post-u")
	if selection.FirstKeptID != "real-a" {
		t.Fatalf("first kept ID = %q, want real active-path ID", selection.FirstKeptID)
	}
}

func TestSelectCompactionNoopWhenEverythingWouldBeRetained(t *testing.T) {
	messages := []model.Message{
		selectTextMessage("u1", model.RoleUser, "request"),
		selectTextMessage("a1", model.RoleAssistant, "answer"),
	}
	if _, err := selectCompaction(messages, session.CompactionMetadata{}, false, 100000, 0); !errors.Is(err, ErrNothingToCompact) {
		t.Fatalf("error = %v, want ErrNothingToCompact", err)
	}
}

func TestSelectCompactionEnforcesHardRetainedBudget(t *testing.T) {
	messages := []model.Message{
		selectTextMessage("old-u", model.RoleUser, selectLongText(180)),
		selectTextMessage("old-a", model.RoleAssistant, selectLongText(180)),
		selectTextMessage("latest-u", model.RoleUser, "current request"),
		selectTextMessage("latest-a", model.RoleAssistant, "current answer"),
	}
	forced := estimateMessage(messages[2]) + estimateMessage(messages[3])

	if _, err := selectCompaction(messages, session.CompactionMetadata{}, false, 100000, forced-1); !errors.Is(err, ErrCurrentTurnTooLarge) {
		t.Fatalf("error = %v, want ErrCurrentTurnTooLarge", err)
	}

	selection, err := selectCompaction(messages, session.CompactionMetadata{}, false, 100000, forced)
	if err != nil {
		t.Fatal(err)
	}
	if got := selectMessageTokens(selection.Retained); got > forced {
		t.Fatalf("retained estimate = %d, hard budget = %d", got, forced)
	}
	assertSelectIDs(t, selection.Retained, "latest-u", "latest-a")
}

func TestSelectCompactionRetainedTailAssistantAnchorSplitsAtPriorUser(t *testing.T) {
	messages := []model.Message{
		selectCompactionMessage("checkpoint", "[Compaction summary]\nprior"),
		selectTextMessage("history-u", model.RoleUser, "historical request"),
		selectTextMessage("history-a", model.RoleAssistant, "historical answer"),
		selectTextMessage("turn-u", model.RoleUser, "turn request"),
		selectTextMessage("post-assistant", model.RoleAssistant, "genuine retained answer"),
		selectTextMessage("post-u", model.RoleUser, "later request"),
	}
	latest := session.CompactionMetadata{
		ID:                           "checkpoint",
		RetainedTailOnly:             true,
		FirstPostCheckpointMessageID: "post-assistant",
	}

	selection, err := selectCompaction(messages, latest, true, 1, 0)
	if err != nil {
		t.Fatal(err)
	}
	assertSelectIDs(t, selection.HistoricalSource, "history-u", "history-a")
	assertSelectIDs(t, selection.TurnPrefixSource, "turn-u")
	assertSelectIDs(t, selection.Retained, "post-assistant", "post-u")
	if selection.FirstKeptID != "post-assistant" {
		t.Fatalf("first kept ID = %q, want fixed assistant anchor", selection.FirstKeptID)
	}
	if !selection.SplitTurn {
		t.Fatal("fixed assistant anchor was not marked as a split turn")
	}
}

func TestSelectCompactionContextEdgeCases(t *testing.T) {
	t.Run("context only is a no-op", func(t *testing.T) {
		messages := []model.Message{
			selectContextMessage("ctx-1", "branch_summary", "first"),
			selectContextMessage("ctx-2", "custom", "second"),
		}
		if _, err := selectCompaction(messages, session.CompactionMetadata{}, false, 1, 0); !errors.Is(err, ErrNothingToCompact) {
			t.Fatalf("error = %v, want ErrNothingToCompact", err)
		}
	})

	t.Run("consecutive contexts attach forward", func(t *testing.T) {
		messages := []model.Message{
			selectTextMessage("old-u", model.RoleUser, "old request"),
			selectTextMessage("old-a", model.RoleAssistant, "old answer"),
			selectContextMessage("ctx-1", "branch_summary", "first"),
			selectContextMessage("ctx-2", "custom", "second"),
			selectTextMessage("latest-u", model.RoleUser, "latest request"),
		}
		selection, err := selectCompaction(messages, session.CompactionMetadata{}, false, 1, 0)
		if err != nil {
			t.Fatal(err)
		}
		assertSelectIDs(t, selection.HistoricalSource, "old-u", "old-a")
		assertSelectIDs(t, selection.Retained, "ctx-1", "ctx-2", "latest-u")
	})

	t.Run("trailing contexts are a forced suffix not a prior atom", func(t *testing.T) {
		messages := []model.Message{
			selectTextMessage("old-u", model.RoleUser, "old request"),
			selectTextMessage("old-a", model.RoleAssistant, "old answer"),
			selectTextMessage("latest-u", model.RoleUser, "latest request"),
			selectTextMessage("latest-a", model.RoleAssistant, "latest answer"),
			selectContextMessage("ctx-1", "branch_summary", "first trailing context"),
			selectContextMessage("ctx-2", "custom", "second trailing context"),
		}
		groups, err := groupCompactionMessages(messages)
		if err != nil {
			t.Fatal(err)
		}
		if got := groups[len(groups)-1].end; got != 4 {
			t.Fatalf("last atom ends at %d, want 4 before trailing forced suffix", got)
		}

		selection, err := selectCompaction(messages, session.CompactionMetadata{}, false, 1, 0)
		if err != nil {
			t.Fatal(err)
		}
		assertSelectIDs(t, selection.Retained, "latest-u", "latest-a", "ctx-1", "ctx-2")
		if selection.FirstKeptID != "latest-u" {
			t.Fatalf("first kept ID = %q, want latest-u", selection.FirstKeptID)
		}
		forced := selectMessageEstimate(messages[2:])
		if _, err := selectCompaction(messages, session.CompactionMetadata{}, false, 1, forced-1); !errors.Is(err, ErrCurrentTurnTooLarge) {
			t.Fatalf("error = %v, want trailing suffix included in forced retained budget", err)
		}
	})
}

func TestSelectCompactionUsesLatestOfRepeatedCompactionContexts(t *testing.T) {
	messages := []model.Message{
		selectCompactionMessage("checkpoint-1", "[Compaction summary]\nolder summary"),
		selectTextMessage("old-u", model.RoleUser, "old request"),
		selectTextMessage("old-a", model.RoleAssistant, "old answer"),
		selectCompactionMessage("checkpoint-2", "[Compaction summary]\nlatest summary"),
		selectTextMessage("latest-u", model.RoleUser, "latest request"),
	}
	latest := session.CompactionMetadata{ID: "checkpoint-2", Summary: "metadata fallback"}

	selection, err := selectCompaction(messages, latest, true, 1, 0)
	if err != nil {
		t.Fatal(err)
	}
	if selection.PreviousSummary != "latest summary" {
		t.Fatalf("previous summary = %q, want latest summary", selection.PreviousSummary)
	}
	assertSelectIDs(t, selection.HistoricalSource, "old-u", "old-a")
	assertSelectIDs(t, selection.Retained, "latest-u")
}

func TestSelectCompactionRejectsEmptySelectedFirstKeptID(t *testing.T) {
	tests := []struct {
		name      string
		messages  []model.Message
		latest    session.CompactionMetadata
		hasLatest bool
		keep      int
	}{
		{
			name: "ordinary",
			messages: []model.Message{
				selectTextMessage("old-u", model.RoleUser, "old request"),
				selectTextMessage("old-a", model.RoleAssistant, "old answer"),
				selectTextMessage("", model.RoleUser, "latest request"),
			},
			keep: 1,
		},
		{
			name: "dual form",
			messages: []model.Message{
				selectCompactionMessage("checkpoint", "[Compaction summary]\nprior"),
				selectTextMessage("real-u", model.RoleUser, "real retained request"),
				selectTextMessage("", model.RoleAssistant, "real retained answer"),
				selectTextMessage("post-u", model.RoleUser, "post-checkpoint request"),
			},
			latest: session.CompactionMetadata{
				ID:                           "checkpoint",
				FirstKeptEntryID:             "real-u",
				FirstPostCheckpointMessageID: "post-u",
			},
			hasLatest: true,
			keep:      20,
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			if _, err := selectCompaction(test.messages, test.latest, test.hasLatest, test.keep, 0); !errors.Is(err, ErrNothingToCompact) {
				t.Fatalf("error = %v, want ErrNothingToCompact", err)
			}
		})
	}
}

func TestSelectCompactionAnchorZeroChecksHardBudgetBeforeNoop(t *testing.T) {
	messages := []model.Message{selectTextMessage("post-u", model.RoleUser, selectLongText(180))}
	latest := session.CompactionMetadata{
		RetainedTailOnly:             true,
		FirstPostCheckpointMessageID: "post-u",
	}

	if _, err := selectCompaction(messages, latest, true, 1, estimateMessage(messages[0])-1); !errors.Is(err, ErrCurrentTurnTooLarge) {
		t.Fatalf("error = %v, want ErrCurrentTurnTooLarge", err)
	}
}

func TestSelectCompactionDeepClonesEverySelectedPartition(t *testing.T) {
	messages := []model.Message{
		selectTextMessage("history-u", model.RoleUser, "historical request"),
		selectTextMessage("history-a", model.RoleAssistant, "historical answer"),
		selectTextMessage("turn-u", model.RoleUser, "turn request"),
		selectTextMessage("turn-early", model.RoleAssistant, selectLongText(240)),
		selectTextMessage("turn-late", model.RoleAssistant, "late progress"),
		selectTextMessage("latest-u", model.RoleUser, "follow-up"),
	}
	for index := range messages {
		messages[index].Blocks[0].Arguments = []byte(`{"index":1}`)
		messages[index].Usage = &model.Usage{InputTokens: index + 1}
	}
	original := cloneMessages(messages)
	keep := estimateMessage(messages[4]) + estimateMessage(messages[5])

	selection, err := selectCompaction(messages, session.CompactionMetadata{}, false, keep, 0)
	if err != nil {
		t.Fatal(err)
	}
	partitions := [][]model.Message{selection.HistoricalSource, selection.TurnPrefixSource, selection.Retained}
	for _, partition := range partitions {
		if len(partition) == 0 {
			t.Fatal("test fixture did not populate every selected partition")
		}
		partition[0].Blocks[0].Text = "mutated"
		partition[0].Blocks[0].Arguments[0] = 'X'
		partition[0].Usage.InputTokens = 999
	}
	if !reflect.DeepEqual(messages, original) {
		t.Fatal("mutating selection changed nested input message data")
	}
}

func selectToolConversationFixture() []model.Message {
	return []model.Message{
		selectTextMessage("u1", model.RoleUser, "use both tools"),
		selectToolCallMessage("calls",
			model.Block{Type: model.BlockToolCall, ToolCallID: "call-1", ToolName: "read"},
			model.Block{Type: model.BlockToolCall, ToolCallID: "call-2", ToolName: "write"},
		),
		selectToolResultMessage("result-2", "call-2", "write"),
		selectToolResultMessage("result-1", "call-1", "read"),
		selectTextMessage("answer", model.RoleAssistant, "tools complete"),
		selectTextMessage("u2", model.RoleUser, "continue"),
		selectTextMessage("a2", model.RoleAssistant, "continued"),
	}
}

func selectTextMessage(id string, role model.Role, text string) model.Message {
	return model.Message{ID: id, Role: role, Blocks: []model.Block{{Type: model.BlockText, Text: text}}}
}

func selectContextMessage(id, contextType, text string) model.Message {
	message := selectTextMessage(id, model.RoleContext, text)
	message.ContextType = contextType
	return message
}

func selectCompactionMessage(id, text string) model.Message {
	return selectContextMessage(id, "compaction", text)
}

func selectToolCallMessage(id string, calls ...model.Block) model.Message {
	return model.Message{ID: id, Role: model.RoleAssistant, Blocks: calls, FinishReason: model.FinishToolCalls}
}

func selectToolResultMessage(id, callID, name string) model.Message {
	return model.Message{ID: id, Role: model.RoleTool, Blocks: []model.Block{{
		Type: model.BlockToolResult, ToolCallID: callID, ToolName: name, Text: "result",
	}}}
}

func selectLongText(bytes int) string {
	result := make([]byte, bytes)
	for index := range result {
		result[index] = 'x'
	}
	return string(result)
}

func selectMessageTokens(messages []model.Message) int {
	total := 0
	for _, message := range messages {
		total = saturatingEstimateAdd(total, estimateMessage(message))
	}
	return total
}

func assertSelectIDs(t *testing.T, messages []model.Message, want ...string) {
	t.Helper()
	got := make([]string, len(messages))
	for index, message := range messages {
		got[index] = message.ID
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("message IDs = %v, want %v", got, want)
	}
}

func assertToolAtomSide(t *testing.T, selection compactionSelection, ids ...string) {
	t.Helper()
	sides := map[string]string{}
	for _, part := range []struct {
		name     string
		messages []model.Message
	}{
		{name: "historical", messages: selection.HistoricalSource},
		{name: "turn-prefix", messages: selection.TurnPrefixSource},
		{name: "retained", messages: selection.Retained},
	} {
		for _, message := range part.messages {
			sides[message.ID] = part.name
		}
	}
	wantSide := sides[ids[0]]
	for _, id := range ids[1:] {
		if sides[id] != wantSide {
			t.Fatalf("tool atom split: %s is %s, want %s (%v)", id, sides[id], wantSide, sides)
		}
	}
	if wantSide == "" {
		t.Fatalf("tool atom absent from selection: %v", sides)
	}
}
