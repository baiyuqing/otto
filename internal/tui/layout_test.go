package tui

import (
	"fmt"
	"math"
	"strconv"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/model"
)

func TestResponsiveHelpOverlayDescribesDetailToggle(t *testing.T) {
	content := helpOverlayContent(100, 20, app.SandboxInfo{
		Mode: app.SandboxOff, Network: app.SandboxNetworkUnconfined, BashAvailable: true, Reason: app.SandboxReasonNone,
	})
	if !strings.Contains(content, "toggle details") {
		t.Fatalf("help overlay = %q, want detail toggle wording", content)
	}
	if strings.Contains(content, "toggle tool arguments/output") {
		t.Fatalf("help overlay = %q, want old tool-only wording removed", content)
	}
}

func TestFormatFooterTokenCountBoundaries(t *testing.T) {
	tests := []struct {
		name   string
		tokens int
		want   string
	}{
		{name: "negative clamps to zero", tokens: -1, want: "0"},
		{name: "zero stays zero", tokens: 0, want: "0"},
		{name: "sub-thousand stays raw", tokens: 999, want: "999"},
		{name: "one thousand becomes k", tokens: 1000, want: "1k"},
		{name: "rounds down below half tenth", tokens: 1049, want: "1k"},
		{name: "rounds up at half tenth", tokens: 1050, want: "1.1k"},
		{name: "rounds to one and three tenths", tokens: 1250, want: "1.3k"},
		{name: "keeps largest sub-k boundary under a million", tokens: 999949, want: "999.9k"},
		{name: "promotes rounded million boundary", tokens: 999950, want: "1M"},
		{name: "one million becomes M", tokens: 1000000, want: "1M"},
		{name: "millions keep one decimal", tokens: 1250000, want: "1.3M"},
		{name: "promotes rounded billion boundary", tokens: 999950000, want: "1B"},
		{name: "billions keep one decimal", tokens: 1250000000, want: "1.3B"},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if got := formatFooterTokenCount(tt.tokens); got != tt.want {
				t.Fatalf("formatFooterTokenCount(%d) = %q, want %q", tt.tokens, got, tt.want)
			}
		})
	}
}

func TestFormatFooterTokenCountMaxIntBoundaries(t *testing.T) {
	want := "9223372036.9B"
	if strconv.IntSize == 32 {
		want = "2.1B"
	}
	if got := formatFooterTokenCount(math.MaxInt); got != want {
		t.Fatalf("formatFooterTokenCount(math.MaxInt) = %q, want %q", got, want)
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

func TestRenderFooterShowsContextPercentageStates(t *testing.T) {
	base := app.Info{Profile: "profile", Model: "model", SessionID: "session", ContextWindow: 128_000}
	tests := []struct {
		name        string
		info        app.Info
		want        string
		wantMissing []string
	}{
		{name: "known latest context", info: app.Info{Profile: base.Profile, Model: base.Model, SessionID: base.SessionID, ContextWindow: base.ContextWindow, ContextInputTokens: 29_952, ContextInputTokensPresent: true}, want: "ctx 23.4%"},
		{name: "pending after compaction", info: app.Info{Profile: base.Profile, Model: base.Model, SessionID: base.SessionID, ContextWindow: base.ContextWindow, ContextInputTokensPending: true}, want: "ctx ?%"},
		{name: "unknown window hides field", info: app.Info{Profile: base.Profile, Model: base.Model, SessionID: base.SessionID, ContextInputTokens: 29_952, ContextInputTokensPresent: true}, wantMissing: []string{"ctx "}},
		{name: "omitted prompt usage hides field", info: base, wantMissing: []string{"ctx "}},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			footer := renderFooter(120, tt.info, model.Usage{InputTokens: 20, OutputTokens: 6}, "")
			if tt.want != "" && !strings.Contains(footer, tt.want) {
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

func TestFormatFooterContextPercentage(t *testing.T) {
	tests := []struct {
		name   string
		input  int
		window int
		want   string
	}{
		{name: "zero input", input: 0, window: 128_000, want: "0.0%"},
		{name: "rounds to one decimal", input: 1, window: 6, want: "16.7%"},
		{name: "exact percentage", input: 29_952, window: 128_000, want: "23.4%"},
		{name: "over one hundred is not clamped", input: 1_100, window: 1_000, want: "110.0%"},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if got := formatFooterContextPercentage(tt.input, tt.window); got != tt.want {
				t.Fatalf("formatFooterContextPercentage(%d, %d) = %q, want %q", tt.input, tt.window, got, tt.want)
			}
		})
	}
}

func TestRenderFooterContextFieldStaysWithinBounds(t *testing.T) {
	info := app.Info{Profile: "profile", Model: "model", SessionID: "session", ContextWindow: 128_000, ContextInputTokens: 29_952, ContextInputTokensPresent: true}
	for _, width := range []int{48, 60, 72, 120} {
		t.Run(fmt.Sprintf("width-%d", width), func(t *testing.T) {
			footer := renderFooter(width, info, model.Usage{InputTokens: 20, OutputTokens: 6}, "status")
			assertRenderedBounds(t, footer, width, 1)
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

func TestRenderFooterKeepsSandboxBadgeAcrossSupportedWidths(t *testing.T) {
	states := []struct {
		name  string
		info  app.SandboxInfo
		badge string
	}{
		{
			name:  "seatbelt",
			info:  app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: app.SandboxNetworkAllowed, BashAvailable: true, Reason: app.SandboxReasonNone},
			badge: "sb",
		},
		{
			name:  "off",
			info:  app.SandboxInfo{Mode: app.SandboxOff, Network: app.SandboxNetworkUnconfined, BashAvailable: true, Reason: app.SandboxReasonNone},
			badge: "unsafe",
		},
		{
			name:  "unavailable",
			info:  app.SandboxInfo{Mode: app.SandboxUnavailable, Network: app.SandboxNetworkDenied, BashAvailable: false, Reason: app.SandboxReasonSelfTestFailed},
			badge: "no-bash",
		},
	}

	for _, state := range states {
		for _, width := range []int{minTerminalWidth, 48, 60, 72, 120} {
			t.Run(fmt.Sprintf("%s-width-%d", state.name, width), func(t *testing.T) {
				info := app.Info{
					Workspace: "/workspace/" + strings.Repeat("workspace", 20),
					Profile:   "profile", Model: "model", SessionID: strings.Repeat("session", 20),
					ContextWindow: 128_000, ContextInputTokens: 100_000, ContextInputTokensPresent: true,
					Sandbox: state.info,
				}
				footer := renderFooter(width, info, model.Usage{InputTokens: 123_456, OutputTokens: 78_901, CachedInputTokens: 100_000}, "working")
				assertRenderedBounds(t, footer, width, 1)
				fields := strings.Split(strings.TrimSpace(footer), " | ")
				found := false
				for _, field := range fields {
					if field == state.badge {
						found = true
					}
				}
				if !found {
					t.Fatalf("footer = %q, want fixed Sandbox badge %q", footer, state.badge)
				}
			})
		}
	}
}

func TestSandboxOverlayContentUsesOnlyFixedSafeState(t *testing.T) {
	payload := "invalid\x1b]52;c;owned\a\nforged"
	sandboxInfo := app.SandboxInfo{
		Mode: app.SandboxMode(payload), Network: app.SandboxNetwork(payload), BashAvailable: true, Reason: app.SandboxReason(payload),
	}
	help := helpOverlayContent(100, 30, sandboxInfo)
	session := sessionOverlayContent(app.Info{SessionID: "session", Sandbox: sandboxInfo})
	for name, content := range map[string]string{"help": help, "session": session} {
		if !strings.Contains(content, "Sandbox: bash disabled · sandbox unavailable") {
			t.Fatalf("%s content = %q, want safe Sandbox summary", name, content)
		}
		if strings.Contains(content, payload) || strings.ContainsAny(content, "\x1b\a") {
			t.Fatalf("%s content leaked control-bearing Sandbox state: %q", name, content)
		}
	}
	if !strings.Contains(session, "Sandbox reason: runtime-failure") {
		t.Fatalf("session content = %q, want safe fallback reason", session)
	}
	if strings.Contains(help, "Sandbox reason:") {
		t.Fatalf("help content exposed unavailable reason: %q", help)
	}

	for _, size := range []struct{ width, height int }{{40, 8}, {60, 12}, {100, 30}} {
		assertRenderedBounds(t, renderOverlay(size.width, size.height, helpOverlayContent(size.width, size.height, sandboxInfo)), size.width, size.height)
		assertRenderedBounds(t, renderOverlay(size.width, size.height, session), size.width, size.height)
	}
}

func TestEmptyTranscriptHintStaysWithinBounds(t *testing.T) {
	for _, width := range []int{minTerminalWidth, 60, 100} {
		t.Run(fmt.Sprintf("width-%d", width), func(t *testing.T) {
			hint := emptyTranscriptHint(width)
			if hint == "" {
				t.Fatalf("hint = %q, want non-empty", hint)
			}
			assertRenderedBounds(t, hint, width, 5)
		})
	}
}
