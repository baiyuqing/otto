package bridge

import (
	"context"
	"encoding/json"
	"github.com/baiyuqing/otto/chat-connector/internal/agent/fake"
	"slices"
	"strings"
	"testing"
)

func TestLocalCommandReceiptIsSeparatePlainTextAndNeverPromptsModel(t *testing.T) {
	t.Parallel()
	h := newHarness(t, setup{})
	h.say("hello")
	h.waitSent(1)
	sid := h.store.Session("fake:c1")
	callsBefore := slices.Clone(fake.Calls(h.dir))
	params, err := json.Marshal(map[string]any{"sessionId": sid, "receipts": []map[string]any{{
		"id": "invocation-1", "command": "/memory show", "status": "success", "summary": "full-memory-id revision=3",
		"parameters": "id=full-memory-id", "output": "query details",
	}}})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := h.b.Extension(context.Background(), "_otto/local_commands/update", params); err != nil {
		t.Fatal(err)
	}
	h.waitSent(2)
	h.plat.mu.Lock()
	receipt := h.plat.sent[1]
	h.plat.mu.Unlock()
	if receipt.markdown || !strings.Contains(receipt.text, "›_ LOCAL COMMAND ✓ /memory show") || !strings.Contains(receipt.text, "query details") {
		t.Fatalf("receipt = %+v", receipt)
	}
	if calls := fake.Calls(h.dir); !slices.Equal(calls, callsBefore) {
		t.Fatalf("receipt prompted model: %q", calls)
	}
}
