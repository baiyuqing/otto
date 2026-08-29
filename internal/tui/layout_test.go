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
