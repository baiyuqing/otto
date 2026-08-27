package session

import (
	"encoding/json"
	"errors"
	"reflect"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/model"
)

func TestBuildContextUsesOnlyActiveRootToLeafPath(t *testing.T) {
	file := readPiFixture(t, "tree.jsonl")
	context, warnings, err := buildContext(file.Entries, file.Entries[len(file.Entries)-1].ID)
	if err != nil {
		t.Fatal(err)
	}
	if len(warnings) != 0 {
		t.Fatalf("warnings = %#v", warnings)
	}
	assertMessageTexts(t, context.Messages, []string{
		"root",
		"[Branch summary]\ninactive work was abandoned",
		"active branch",
	})
	for _, message := range context.Messages {
		if strings.Contains(message.Text(), "inactive branch") || strings.Contains(message.Text(), "inactive custom") {
			t.Fatalf("inactive branch leaked into context: %#v", context.Messages)
		}
	}
}

func TestBuildContextUsesRetainedTailCompactionCheckpoint(t *testing.T) {
	context := contextFromFixture(t, "compacted.jsonl")
	assertMessageTexts(t, context.Messages, []string{"[Compaction summary]\nsummary", "retained", "after"})
	if context.Messages[0].Role != model.RoleContext || !context.Messages[0].Display || context.Messages[0].ContextType != "compaction" {
		t.Fatalf("compaction message = %#v", context.Messages[0])
	}
	if context.Usage.InputTokens != 12 || context.Usage.OutputTokens != 4 {
		t.Fatalf("usage = %#v", context.Usage)
	}
}

func TestBuildContextUsesLegacyFirstKeptEntryID(t *testing.T) {
	file := readPiFixture(t, "compacted.jsonl")
	context, warnings, err := buildContext(file.Entries, "c0000003")
	if err != nil {
		t.Fatal(err)
	}
	if len(warnings) != 0 {
		t.Fatalf("warnings = %#v", warnings)
	}
	assertMessageTexts(t, context.Messages, []string{"[Compaction summary]\nlegacy summary", "legacy retained"})

	missing := file.Entries[2]
	missing.Compaction.FirstKeptEntryID = stringPointer("ffffffff")
	_, _, err = buildContext([]piEntry{file.Entries[0], file.Entries[1], missing}, missing.ID)
	if !errors.Is(err, ErrInvalidSession) {
		t.Fatalf("missing firstKeptEntryId error = %v, want ErrInvalidSession", err)
	}
}

func TestBuildContextRejectsImageOnActiveBranchOnly(t *testing.T) {
	root := testPiUserEntry("10000001", nil, "root")
	image := mustDecodeContextEntry(t, `{"type":"message","id":"10000002","parentId":"10000001","timestamp":"2026-08-27T12:00:02Z","message":{"role":"user","content":[{"type":"image","data":"aW1hZ2U=","mimeType":"image/png"}],"timestamp":2}}`)
	textParent := root.ID
	text := testPiUserEntry("10000003", &textParent, "text leaf")
	entries := []piEntry{root, image, text}

	_, _, err := buildContext(entries, image.ID)
	if !errors.Is(err, ErrUnsupportedSessionContent) {
		t.Fatalf("active image error = %v, want ErrUnsupportedSessionContent", err)
	}
	context, _, err := buildContext(entries, text.ID)
	if err != nil {
		t.Fatal(err)
	}
	assertMessageTexts(t, context.Messages, []string{"root", "text leaf"})
}

func TestBuildContextRejectsUnsupportedMessageOnActiveBranchOnly(t *testing.T) {
	bash := mustDecodeContextEntry(t, `{"type":"message","id":"20000001","parentId":null,"timestamp":"2026-08-27T12:00:01Z","message":{"role":"bashExecution","command":"pwd","output":"/workspace","exitCode":0,"cancelled":false,"truncated":false,"timestamp":1}}`)
	text := testPiUserEntry("20000002", nil, "supported root")
	entries := []piEntry{bash, text}

	if _, _, err := buildContext(entries, bash.ID); !errors.Is(err, ErrUnsupportedSessionContent) {
		t.Fatalf("active bash error = %v, want ErrUnsupportedSessionContent", err)
	}
	if _, _, err := buildContext(entries, text.ID); err != nil {
		t.Fatal(err)
	}
}

func TestBuildContextRejectsProviderRequiredContentOnActiveBranchOnly(t *testing.T) {
	providerSpecific := mustDecodeContextEntry(t, `{"type":"message","id":"21000001","parentId":null,"timestamp":"2026-08-27T12:00:01Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call-1","name":"read","arguments":{"path":"README.md"},"thoughtSignature":"provider-state"}],"api":"provider-api","provider":"provider","model":"model","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"toolUse","timestamp":1}}`)
	supported := testPiUserEntry("21000002", nil, "supported root")
	entries := []piEntry{providerSpecific, supported}

	if _, _, err := buildContext(entries, providerSpecific.ID); !errors.Is(err, ErrUnsupportedSessionContent) {
		t.Fatalf("active provider content error = %v, want ErrUnsupportedSessionContent", err)
	}
	if _, _, err := buildContext(entries, supported.ID); err != nil {
		t.Fatal(err)
	}
}

func TestBuildContextRuntimePrecedenceAndActiveMetadata(t *testing.T) {
	file := readPiFixture(t, "tree.jsonl")
	context, _, err := buildContext(file.Entries, file.Entries[len(file.Entries)-1].ID)
	if err != nil {
		t.Fatal(err)
	}
	if want := (RuntimeMetadata{Profile: "default", Provider: "openai-compatible", Model: "test-model"}); context.Runtime != want {
		t.Fatalf("runtime = %#v, want %#v", context.Runtime, want)
	}
	if context.ThinkingLevel != "high" || context.SessionName != "Fixture tree" {
		t.Fatalf("thinking/name = %q/%q", context.ThinkingLevel, context.SessionName)
	}

	root := testPiUserEntry("30000001", nil, "root")
	parent := root.ID
	assistant := testPiAssistantEntry("30000002", &parent, "assistant", "assistant-provider", "assistant-model", 3, 2, "stop")
	parent = assistant.ID
	modelChange := testPiEntry("model_change", "30000003", &parent)
	modelChange.ModelChange = &piModelChange{Provider: "changed-provider", ModelID: "changed-model"}
	parent = modelChange.ID
	latestAssistant := testPiAssistantEntry("30000004", &parent, "latest", "later-provider", "later-model", 5, 4, "stop")

	context, _, err = buildContext([]piEntry{root, assistant, modelChange, latestAssistant}, latestAssistant.ID)
	if err != nil {
		t.Fatal(err)
	}
	if want := (RuntimeMetadata{Provider: "changed-provider", Model: "changed-model"}); context.Runtime != want {
		t.Fatalf("model-change runtime = %#v, want %#v", context.Runtime, want)
	}

	context, _, err = buildContext([]piEntry{root, assistant}, assistant.ID)
	if err != nil {
		t.Fatal(err)
	}
	if want := (RuntimeMetadata{Provider: "assistant-provider", Model: "assistant-model"}); context.Runtime != want {
		t.Fatalf("assistant runtime = %#v, want %#v", context.Runtime, want)
	}
}

func TestBuildContextUsesLatestActiveOttoRuntime(t *testing.T) {
	first := testPiRuntimeEntry(t, "31000001", nil, RuntimeMetadata{Profile: "first", Provider: "openai-compatible", Model: "one"})
	parent := first.ID
	second := testPiRuntimeEntry(t, "31000002", &parent, RuntimeMetadata{Profile: "second", Provider: "openai-compatible", Model: "two"})
	parent = second.ID
	change := testPiEntry("model_change", "31000003", &parent)
	change.ModelChange = &piModelChange{Provider: "ignored", ModelID: "ignored"}

	context, _, err := buildContext([]piEntry{first, second, change}, change.ID)
	if err != nil {
		t.Fatal(err)
	}
	if want := (RuntimeMetadata{Profile: "second", Provider: "openai-compatible", Model: "two"}); context.Runtime != want {
		t.Fatalf("runtime = %#v, want %#v", context.Runtime, want)
	}
}

func TestBuildContextFramesBranchAndCustomMessages(t *testing.T) {
	branch := testPiEntry("branch_summary", "40000001", nil)
	branch.BranchSummary = &piBranchSummary{FromID: "ffffffff", Summary: "abandoned work", Usage: testPiUsage(2, 1)}
	parent := branch.ID
	hidden := testPiEntry("custom_message", "40000002", &parent)
	hidden.CustomMessage = &piCustomMessage{CustomType: "hidden.fixture", ContentText: stringPointer("secret context"), Display: false}
	parent = hidden.ID
	visible := testPiEntry("custom_message", "40000003", &parent)
	visible.CustomMessage = &piCustomMessage{CustomType: "visible.fixture", ContentText: stringPointer("shown context"), Display: true}

	context, warnings, err := buildContext([]piEntry{branch, hidden, visible}, visible.ID)
	if err != nil {
		t.Fatal(err)
	}
	if len(warnings) != 0 {
		t.Fatalf("warnings = %#v", warnings)
	}
	assertMessageTexts(t, context.Messages, []string{
		"[Branch summary]\nabandoned work",
		"[Custom context: hidden.fixture]\nsecret context",
		"[Custom context: visible.fixture]\nshown context",
	})
	if context.Messages[0].ContextType != "branch_summary" || !context.Messages[0].Display || context.Messages[1].Display || !context.Messages[2].Display {
		t.Fatalf("context metadata = %#v", context.Messages)
	}
	if context.Usage != (model.Usage{InputTokens: 2, OutputTokens: 1}) {
		t.Fatalf("usage = %#v", context.Usage)
	}
}

func TestBuildContextSupportsCustomAgentMessages(t *testing.T) {
	entry := mustDecodeContextEntry(t, `{"type":"message","id":"41000001","parentId":null,"timestamp":"2026-08-27T12:00:01Z","message":{"role":"custom","customType":"agent.fixture","content":"injected","display":false,"timestamp":1}}`)
	context, _, err := buildContext([]piEntry{entry}, entry.ID)
	if err != nil {
		t.Fatal(err)
	}
	assertMessageTexts(t, context.Messages, []string{"[Custom context: agent.fixture]\ninjected"})
	if context.Messages[0].Display {
		t.Fatalf("custom agent message = %#v", context.Messages[0])
	}
}

func TestBuildContextWarnsForOrphanRootAndMultipleRoots(t *testing.T) {
	missing := "ffffffff"
	orphan := testPiUserEntry("50000001", &missing, "orphan")
	context, warnings, err := buildContext([]piEntry{orphan}, orphan.ID)
	if err != nil {
		t.Fatal(err)
	}
	assertMessageTexts(t, context.Messages, []string{"orphan"})
	assertWarningContains(t, warnings, orphan.ID)

	first := testPiUserEntry("50000002", nil, "first root")
	second := testPiUserEntry("50000003", nil, "second root")
	context, warnings, err = buildContext([]piEntry{first, second}, second.ID)
	if err != nil {
		t.Fatal(err)
	}
	assertMessageTexts(t, context.Messages, []string{"second root"})
	assertWarningContains(t, warnings, "multiple roots")
}

func TestBuildContextWarnsAndIgnoresUnknownActiveEntry(t *testing.T) {
	root := testPiUserEntry("60000001", nil, "root")
	parent := root.ID
	unknown := testPiEntry("future_entry_with_untrusted_\x1b[31m_type", "60000002", &parent)
	parent = unknown.ID
	leaf := testPiUserEntry("60000003", &parent, "leaf")

	context, warnings, err := buildContext([]piEntry{root, unknown, leaf}, leaf.ID)
	if err != nil {
		t.Fatal(err)
	}
	assertMessageTexts(t, context.Messages, []string{"root", "leaf"})
	assertWarningContains(t, warnings, unknown.ID)
	for _, warning := range warnings {
		if strings.Contains(warning.Message, "\x1b") || len(warning.Message) > 256 {
			t.Fatalf("unsafe or unbounded warning = %q", warning.Message)
		}
	}

	inactiveUnknown := testPiEntry("future_entry", "60000004", nil)
	if _, warnings, err := buildContext([]piEntry{inactiveUnknown, root}, root.ID); err != nil || len(warnings) != 1 {
		// The two explicit roots produce exactly the multiple-root warning; the inactive
		// unknown entry must not add another warning.
		t.Fatalf("inactive unknown result: warnings=%#v error=%v", warnings, err)
	}
}

func TestBuildContextPreservesToolCallResultPairing(t *testing.T) {
	context := contextFromFixture(t, "linear.jsonl")
	if len(context.Messages) != 4 {
		t.Fatalf("messages = %#v", context.Messages)
	}
	call := context.Messages[1].Blocks[1]
	result := context.Messages[2].Blocks[0]
	if call.Type != model.BlockToolCall || result.Type != model.BlockToolResult || call.ToolCallID != result.ToolCallID || call.ToolName != result.ToolName {
		t.Fatalf("tool pair = %#v / %#v", call, result)
	}
}

func TestBuildContextRejectsPiPendingAssistant(t *testing.T) {
	pending := testPiAssistantEntry("70000001", nil, "partial", "openai-compatible", "model", 0, 0, "pending")
	_, _, err := buildContext([]piEntry{pending}, pending.ID)
	if !errors.Is(err, ErrInvalidSession) {
		t.Fatalf("error = %v, want ErrInvalidSession", err)
	}
}

func TestBuildContextRejectsDuplicateForwardParentAndCycle(t *testing.T) {
	root := testPiUserEntry("80000001", nil, "root")
	duplicate := testPiUserEntry(root.ID, nil, "duplicate")
	if _, _, err := buildContext([]piEntry{root, duplicate}, duplicate.ID); !errors.Is(err, ErrInvalidSession) {
		t.Fatalf("duplicate error = %v", err)
	}

	futureID := "80000003"
	forward := testPiUserEntry("80000002", &futureID, "forward")
	future := testPiUserEntry(futureID, nil, "future")
	if _, _, err := buildContext([]piEntry{forward, future}, future.ID); !errors.Is(err, ErrInvalidSession) {
		t.Fatalf("forward error = %v", err)
	}

	selfID := "80000004"
	cycle := testPiUserEntry(selfID, &selfID, "cycle")
	if _, _, err := buildContext([]piEntry{cycle}, cycle.ID); !errors.Is(err, ErrInvalidSession) {
		t.Fatalf("cycle error = %v", err)
	}
}

func TestBuildContextUsageComesFromFullActivePath(t *testing.T) {
	assistant := testPiAssistantEntry("90000001", nil, "answer", "openai-compatible", "model", 10, 3, "stop")
	parent := assistant.ID
	branch := testPiEntry("branch_summary", "90000002", &parent)
	branch.BranchSummary = &piBranchSummary{FromID: assistant.ID, Summary: "summary", Usage: testPiUsage(4, 2)}
	parent = branch.ID
	compaction := testPiEntry("compaction", "90000003", &parent)
	compaction.Compaction = &piCompaction{Summary: "compact", TokensBefore: 10, RetainedTail: []piMessage{}, Usage: testPiUsage(6, 1)}

	context, _, err := buildContext([]piEntry{assistant, branch, compaction}, compaction.ID)
	if err != nil {
		t.Fatal(err)
	}
	if want := (model.Usage{InputTokens: 20, OutputTokens: 6}); context.Usage != want {
		t.Fatalf("usage = %#v, want %#v", context.Usage, want)
	}
}

func contextFromFixture(t *testing.T, name string) ResolvedContext {
	t.Helper()
	file := readPiFixture(t, name)
	context, warnings, err := buildContext(file.Entries, file.Entries[len(file.Entries)-1].ID)
	if err != nil {
		t.Fatal(err)
	}
	if len(warnings) != 0 {
		t.Fatalf("warnings = %#v", warnings)
	}
	return context
}

func assertMessageTexts(t *testing.T, messages []model.Message, want []string) {
	t.Helper()
	got := make([]string, len(messages))
	for i, message := range messages {
		got[i] = message.Text()
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("message texts = %#v, want %#v", got, want)
	}
}

func assertWarningContains(t *testing.T, warnings []Warning, text string) {
	t.Helper()
	for _, warning := range warnings {
		if strings.Contains(warning.Message, text) {
			return
		}
	}
	t.Fatalf("warnings = %#v, want text %q", warnings, text)
}

func mustDecodeContextEntry(t *testing.T, raw string) piEntry {
	t.Helper()
	entry, err := decodePiEntry([]byte(raw))
	if err != nil {
		t.Fatal(err)
	}
	return entry
}

func testPiEntry(entryType, id string, parentID *string) piEntry {
	return piEntry{piEntryBase: piEntryBase{
		Type: entryType, ID: id, ParentID: cloneStringPointer(parentID), Timestamp: "2026-08-27T12:00:00Z",
	}}
}

func testPiUserEntry(id string, parentID *string, text string) piEntry {
	entry := testPiEntry("message", id, parentID)
	entry.Message = &piMessage{Role: "user", ContentText: stringPointer(text), Timestamp: 1}
	return entry
}

func testPiAssistantEntry(id string, parentID *string, text, provider, modelID string, input, output int64, stopReason string) piEntry {
	entry := testPiEntry("message", id, parentID)
	entry.Message = &piMessage{
		Role: "assistant", Provider: provider, Model: modelID, StopReason: stopReason,
		ContentBlocks: []piContentBlock{{Type: "text", Text: text}}, Usage: testPiUsage(input, output), Timestamp: 1,
	}
	return entry
}

func testPiRuntimeEntry(t *testing.T, id string, parentID *string, metadata RuntimeMetadata) piEntry {
	t.Helper()
	entry := testPiEntry("custom", id, parentID)
	entry.Custom = &piCustom{CustomType: ottoRuntimeCustomType, Data: mustMarshalJSON(t, metadata)}
	return entry
}

func testPiUsage(input, output int64) *piUsage {
	return &piUsage{Input: input, Output: output, TotalTokens: input + output}
}

func mustMarshalJSON(t *testing.T, value any) []byte {
	t.Helper()
	encoded, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	return encoded
}
