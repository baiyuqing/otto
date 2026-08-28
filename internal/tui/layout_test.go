package tui

import (
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/model"
)

func TestRenderFooterShowsCachedTokensWhenPresent(t *testing.T) {
	info := app.Info{Profile: "profile", Model: "model", SessionID: "session"}

	footer := renderFooter(120, info, model.Usage{InputTokens: 20, OutputTokens: 6, CachedInputTokens: 15}, "")
	if !strings.Contains(footer, "tokens 20/6 (cached 15)") {
		t.Fatalf("footer = %q, want cached tokens field", footer)
	}

	footer = renderFooter(120, info, model.Usage{InputTokens: 20, OutputTokens: 6}, "")
	if !strings.Contains(footer, "tokens 20/6") || strings.Contains(footer, "cached") {
		t.Fatalf("footer = %q, want plain tokens field without cached", footer)
	}
}
