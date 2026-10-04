package agent

import (
	"encoding/json"
	"testing"
)

func TestMemoryPageDecodesAdvice(t *testing.T) {
	var page MemoryPage
	in := `{"candidates":[{"id":"a","advice":"Advice (x): y"},{"id":"b"}]}`
	if err := json.Unmarshal([]byte(in), &page); err != nil {
		t.Fatal(err)
	}
	if page.Candidates[0].Advice != "Advice (x): y" || page.Candidates[1].Advice != "" {
		t.Fatalf("advice = %q, %q", page.Candidates[0].Advice, page.Candidates[1].Advice)
	}
}
