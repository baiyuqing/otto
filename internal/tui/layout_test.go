package tui

import (
	"fmt"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/model"
)

func TestResponsiveHelpOverlayDescribesDetailToggle(t *testing.T) {
	content := helpOverlayContent(100, 20)
	if !strings.Contains(content, "toggle details") {
		t.Fatalf("help overlay = %q, want detail toggle wording", content)
	}
	if strings.Contains(content, "toggle tool arguments/output") {
		t.Fatalf("help overlay = %q, want old tool-only wording removed", content)
	}
}

func TestRenderFooterUsesHumanReadableTokenTotals(t *testing.T) {
	info := app.Info{Profile: "profile", Model: "model", SessionID: "session"}
	tests := []struct {
		name        string
		usage       model.Usage
		want        string
		wantMissing []string
	}{
		{name: "compacts totals and cached counts", usage: model.Usage{InputTokens: 1250, OutputTokens: 56800, CachedInputTokens: 45700}, want: "tokens 1.3k/56.8k (cached 45.7k)"},
		{name: "keeps zero usage explicit", usage: model.Usage{}, want: "tokens 0/0", wantMissing: []string{"cached"}},
		{name: "omits nonpositive cached totals", usage: model.Usage{InputTokens: 1250, OutputTokens: 56800, CachedInputTokens: -1}, want: "tokens 1.3k/56.8k", wantMissing: []string{"cached"}},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			footer := renderFooter(120, info, tt.usage, "")
			if !strings.Contains(footer, tt.want) {
				t.Fatalf("footer = %q, want %q", footer, tt.want)
			}
			for _, missing := range tt.wantMissing {
				if strings.Contains(footer, missing) {
					t.Fatalf("footer = %q, want missing %q", footer, missing)
				}
			}
		})
	}
}

func TestRenderFooterKeepsZeroUsageRenderableUnderPTYFixtures(t *testing.T) {
	info := app.Info{Profile: "profile", Model: "model", SessionID: "session"}
	footer := renderFooter(120, info, model.Usage{}, "")
	if !strings.Contains(footer, "tokens 0/0") {
		t.Fatalf("footer = %q, want zero-usage tokens marker", footer)
	}
}

func TestRenderFooterHandlesMaxIntUsageWithoutOverflow(t *testing.T) {
	info := app.Info{Profile: "profile", Model: "model", SessionID: "session"}
	footer := renderFooter(120, info, model.Usage{InputTokens: int(^uint(0) >> 1), OutputTokens: int(^uint(0) >> 1)}, "")
	if !strings.Contains(footer, "tokens ") || !strings.Contains(footer, "B") {
		t.Fatalf("footer = %q, want large-token B suffix without panic", footer)
	}
}

func TestEmptyTranscriptHintStaysWithinBounds(t *testing.T) {
	for _, width := range []int{minTerminalWidth, 60, 100} {
		t.Run(fmt.Sprintf("width-%d", width), func(t *testing.T) {
			hint := emptyTranscriptHint(width)
			if hint == "" {
				t.Fatalf("hint = %q, want non-empty", hint)
			}
			assertRenderedBounds(t, hint, width, 1)
		})
	}
}
